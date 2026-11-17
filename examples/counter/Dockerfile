# syntax=docker/dockerfile:1
# Run ./download.sh first, then build --target service.
# Local ADD extracts each tarball into the image root.

# The snapshot build needs only application files, not the updater or s6.
FROM alpine:3.22 AS app
ARG NUMBER=1
ARG HEALTHY=yes
WORKDIR /app
COPY run.sh /app/run
RUN chmod 0755 /app/run \
    && printf '%s\n' "$NUMBER" > /app/number \
    && printf '%s\n' "$HEALTHY" > /app/healthy

FROM app AS service
LABEL org.opencontainers.image.source="https://github.com/btvn-tuan-nd/container-mutator"
ARG RUNTIME_ID=counter-alpine3.22-v1
ARG TARGETARCH
RUN apk add --no-cache ca-certificates
ADD downloads/${TARGETARCH}/s6-overlay-noarch.tar.xz /
ADD downloads/${TARGETARCH}/s6-overlay-arch.tar.xz /
ADD downloads/${TARGETARCH}/mutator.tar.gz /
COPY rootfs/ /
RUN set -eux; \
    find /etc/s6-overlay/s6-rc.d/counter -type d -exec chmod 0755 {} \; ; \
    find /etc/s6-overlay/s6-rc.d/counter -type f -exec chmod 0644 {} \; ; \
    chmod 0755 \
      /etc/s6-overlay/s6-rc.d/counter/run \
      /etc/s6-overlay/s6-rc.d/counter/finish \
      /etc/s6-overlay/s6-rc.d/counter/data/check
ENV S6_BEHAVIOUR_IF_STAGE2_FAILS=2
ENV MUTATOR_RUNTIME=$RUNTIME_ID
ENTRYPOINT ["/init"]

# Unified single-layer /app snapshot: app/ is the layer root.
FROM app AS snapshot
RUN tar -C / -cf /app.tar app

FROM scratch AS payload
COPY --from=snapshot /app.tar /app.tar
