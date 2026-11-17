# syntax=docker/dockerfile:1
# Run ./download.sh first, then build --target service.
# Local ADD extracts each tarball into the image root.

# The snapshot build needs only application files, not the updater or s6.
FROM alpine:3.22 AS app
ARG NUMBER=1
ARG HEALTHY=yes
WORKDIR /srv/site
RUN printf '%s\n' "$NUMBER" > /srv/site/index.html \
    && case "$HEALTHY" in \
         yes) ;; \
         no) rm /srv/site/index.html ;; \
         *) echo "HEALTHY must be yes or no" >&2; exit 1 ;; \
       esac

FROM app AS service
LABEL org.opencontainers.image.source="https://github.com/btvn-tuan-nd/container-mutator"
ARG RUNTIME_ID=counter-nginx-alpine3.22-v1
ARG TARGETARCH
RUN apk add --no-cache ca-certificates nginx
ADD downloads/${TARGETARCH}/s6-overlay-noarch.tar.xz /
ADD downloads/${TARGETARCH}/s6-overlay-arch.tar.xz /
ADD downloads/${TARGETARCH}/mutator.tar.gz /
COPY nginx.conf /etc/nginx/nginx.conf
ENV MUTATOR_APP_DIR=/srv/site
ENV MUTATOR_COMMAND="exec /usr/sbin/nginx -g 'daemon off;'" \
    MUTATOR_CHECK="wget -q -O /dev/null http://127.0.0.1:8080/"
ENV S6_BEHAVIOUR_IF_STAGE2_FAILS=2
ENV MUTATOR_RUNTIME=$RUNTIME_ID
EXPOSE 8080
ENTRYPOINT ["/init"]
CMD []

# Unified single-layer /app snapshot: app/ is the layer root.
FROM app AS snapshot
RUN mkdir /snapshot && cp -a /srv/site /snapshot/app \
    && tar -C /snapshot -cf /app.tar app

FROM scratch AS payload
COPY --from=snapshot /app.tar /app.tar
