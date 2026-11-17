# syntax=docker/dockerfile:1
# Alpine service image for the crane-based publish demo.
# `service` already contains s6-overlay, /init, and the native counter service
# definitions; publish.sh appends the mutator integration tarball at runtime.
# Build arguments: NUMBER (default 1), HEALTHY (default yes).
FROM alpine:3.22 AS s6-overlay
ARG TARGETARCH
RUN apk add --no-cache curl xz
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) arch=x86_64; checksum=e6befcc96a437a3831386ecfc51808c5d3e939dc5fe3c02ae9284599e8aa2408 ;; \
      arm64) arch=aarch64; checksum=b17f17a82e7a515c682a91edaf2ffdabb73f891981b6c1fd712115693a2f8b4c ;; \
      *) echo "Unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    curl --fail --location --show-error --silent \
      "https://github.com/just-containers/s6-overlay/releases/download/v3.2.3.2/s6-overlay-${arch}.tar.xz" \
      --output /tmp/s6-overlay-arch.tar.xz; \
    printf '%s  %s\n' "$checksum" /tmp/s6-overlay-arch.tar.xz | sha256sum -c -; \
    curl --fail --location --show-error --silent \
      "https://github.com/just-containers/s6-overlay/releases/download/v3.2.3.2/s6-overlay-noarch.tar.xz" \
      --output /tmp/s6-overlay-noarch.tar.xz; \
    printf '%s  %s\n' 5379750ed30a84bbd2e2dd74847ba6b5bd29cd0b2e3ea2ec58049b57eb2eda12 /tmp/s6-overlay-noarch.tar.xz | sha256sum -c -; \
    mkdir -p /rootfs; \
    tar -C /rootfs -Jxpf /tmp/s6-overlay-noarch.tar.xz; \
    tar -C /rootfs -Jxpf /tmp/s6-overlay-arch.tar.xz

FROM alpine:3.22 AS service
ARG NUMBER=1
ARG HEALTHY=yes
ARG RUNTIME_ID=counter-alpine3.22-v1
RUN apk add --no-cache ca-certificates
COPY --from=s6-overlay /rootfs/ /
WORKDIR /app
COPY run.sh /app/run
COPY rootfs/ /
RUN set -eux; \
    find /etc/s6-overlay/s6-rc.d/counter -type d -exec chmod 0755 {} \; ; \
    find /etc/s6-overlay/s6-rc.d/counter -type f -exec chmod 0644 {} \; ; \
    chmod 0755 /app/run \
      /etc/s6-overlay/s6-rc.d/counter/run \
      /etc/s6-overlay/s6-rc.d/counter/finish \
      /etc/s6-overlay/s6-rc.d/counter/data/check; \
    printf '%s\n' "$NUMBER" > /app/number; \
    printf '%s\n' "$HEALTHY" > /app/healthy
ENV S6_BEHAVIOUR_IF_STAGE2_FAILS=2
ENV MUTATOR_RUNTIME=$RUNTIME_ID
ENTRYPOINT ["/init"]

# Unified single-layer /app snapshot: app/ is the layer root.
FROM service AS app
RUN tar -C / -cf /app.tar app

FROM scratch AS payload
COPY --from=app /app.tar /app.tar
