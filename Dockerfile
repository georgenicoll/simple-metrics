# The simple-metrics image: the collector, smq and smweb, as static binaries
# in an otherwise empty image (about 2 MB). Nothing is compiled here: the
# release workflow builds the binaries first (so the image holds exactly the
# ones that were tested) and lays them out as
#
#   dist/linux-amd64/{simple-metrics,smq,smweb}
#   dist/linux-arm64/{simple-metrics,smq,smweb}
#
# before running `docker buildx build --platform linux/amd64,linux/arm64`.
#
# Run one container per program, sharing the /run/simple-metrics volume that
# holds the collector's socket; deploy/ in this repository does that. There is
# no default entrypoint: say which binary, e.g. `/simple-metrics --help`.

# Only here to make two directories owned by the unprivileged user: an empty
# image has no shell to do it with, and a new volume takes its ownership from
# the directory in the image.
FROM alpine:3.22 AS prep
RUN mkdir -p /state /run/simple-metrics \
 && chown 65532:65532 /state /run/simple-metrics

FROM scratch
ARG TARGETARCH
LABEL org.opencontainers.image.title="simple-metrics" \
      org.opencontainers.image.description="A small metrics collector and its web chart" \
      org.opencontainers.image.source="https://github.com/georgenicoll/simple-metrics" \
      org.opencontainers.image.licenses="MIT"
COPY --from=prep --chown=65532:65532 /state /state
COPY --from=prep --chown=65532:65532 /run/simple-metrics /run/simple-metrics
COPY --chmod=755 dist/linux-${TARGETARCH}/simple-metrics /simple-metrics
COPY --chmod=755 dist/linux-${TARGETARCH}/smq /smq
COPY --chmod=755 dist/linux-${TARGETARCH}/smweb /smweb
# 65532 is "nobody"'s conventional number in minimal images; nothing here
# needs privileges, so nothing runs as root.
USER 65532:65532
CMD ["/simple-metrics"]
