# syntax=docker/dockerfile:1
#
# container-mutator: one static binary plus minimal s6-rc integration.
#
# Targets:
#   final     (default) Alpine convenience image with pinned s6-overlay and /init.
#   artifact  local export: raw `mutator`, integration and single-app tarballs, SHA256SUMS.
#   overlay   scratch image with only the integration root.
#
# Install the integration artifact into an existing s6-overlay image.
# It adds prepare/watch services, not /init or an application launcher.
# Applications own readiness and cleanup; put `mutable-app` in `user2`.
# s6 retains its upstream license files; crane's license is under
# /opt/mutator/share/licenses/crane-0.22.1.

FROM rust:1-alpine AS build
ARG TARGETARCH
# Explicit target keeps static linking flags off host-side procedural macros.
ENV RUSTFLAGS="-C target-feature=+crt-static"
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-musl ;; \
      arm64) target=aarch64-unknown-linux-musl ;; \
      *) echo "Unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && cargo build --release --locked --target "$target" \
    && mkdir /out \
    && cp "target/$target/release/mutator" /out/mutator

# crane is an external dependency, optionally distributed with mutator.
FROM alpine:3.22 AS crane
ARG TARGETARCH
RUN apk add --no-cache curl
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) crane_arch=x86_64; crane_checksum=0ab7a1d6932a213aed964ce97666c3077fe691c8606413674a8b3e0b9ec4cda0 ;; \
      arm64) crane_arch=arm64; crane_checksum=898c0cff975f898a33e8c4580bdafb0e7c02c7faa33374e946762f97c4ab7110 ;; \
      *) echo "Unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    curl --fail --location --show-error --silent \
      "https://github.com/google/go-containerregistry/releases/download/v0.22.1/go-containerregistry_Linux_${crane_arch}.tar.gz" \
      --output /tmp/crane.tar.gz; \
    printf '%s  %s\n' "$crane_checksum" /tmp/crane.tar.gz | sha256sum -c -; \
    mkdir -p /out/opt/mutator/bin /out/opt/mutator/share/licenses/crane-0.22.1; \
    tar -C /tmp -xzf /tmp/crane.tar.gz crane LICENSE; \
    cp /tmp/crane /out/opt/mutator/bin/crane; \
    cp /tmp/LICENSE /out/opt/mutator/share/licenses/crane-0.22.1/LICENSE; \
    chmod 0755 /out/opt/mutator/bin/crane

# Integration root: binary, crane, and reference s6-rc definitions only.
FROM alpine:3.22 AS integration
COPY --from=build /out/mutator /root/opt/mutator/bin/mutator
COPY --from=crane /out/ /root/
COPY overlay/rootfs/ /root/
RUN set -eux; \
    tree=/root/etc/s6-overlay; \
    find "$tree" -type d -exec chmod 0755 {} \; ; \
    find "$tree" -type f -exec chmod 0644 {} \; ; \
    chmod 0755 "$tree/s6-rc.d/mutator-watch/run" /root/opt/mutator/bin/*; \
    find /root/opt/mutator/share/licenses -type d -exec chmod 0755 {} \; ; \
    find /root/opt/mutator/share/licenses -type f -exec chmod 0644 {} \;

# Single-app preset shares its service templates with install-service.
FROM integration AS single-app
COPY src/service/ /root/etc/s6-overlay/s6-rc.d/mutator-app/
RUN set -eux; \
    service=/root/etc/s6-overlay/s6-rc.d/mutator-app; \
    find "$service" -type d -exec chmod 0755 {} \; ; \
    find "$service" -type f -exec chmod 0644 {} \; ; \
    chmod 0755 "$service/run" "$service/finish" "$service/data/launch" "$service/data/check" "$service/data/dispatch"; \
    touch "$service/data/preset"; \
    bundles=/root/etc/s6-overlay/user-bundles.d; \
    mkdir -p "$bundles/mutable-app/contents.d" "$bundles/user2/contents.d" /root/etc/mutator; \
    printf 'bundle\n' > "$bundles/mutable-app/type"; \
    printf 'bundle\n' > "$bundles/user2/type"; \
    touch "$bundles/mutable-app/contents.d/mutator-app" "$bundles/user2/contents.d/mutable-app"

FROM scratch AS overlay
COPY --from=integration /root/ /

# s6-overlay is installed only by runnable convenience/example images, never
# into the integration artifact.
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

FROM alpine:3.22 AS export
ARG TARGETARCH
COPY --from=build /out/mutator /mutator
COPY --from=integration /root/ /integration/
COPY --from=single-app /root/ /single-app/
RUN set -eux; \
    mkdir -p /out; \
    tarball="container-mutator-integration-linux-${TARGETARCH}.tar.gz"; \
    tar -C /integration -czf "/out/${tarball}" .; \
    tar -C /single-app -czf "/out/container-mutator-single-app-linux-${TARGETARCH}.tar.gz" .; \
    cp /mutator /out/mutator; \
    cd /out; \
    sha256sum *.tar.gz mutator > SHA256SUMS

FROM scratch AS artifact
COPY --from=export /out/ /

FROM alpine:3.22 AS final
# Crane uses the target system trust store; the convenience image ships one.
RUN apk add --no-cache ca-certificates
COPY --from=s6-overlay /rootfs/ /
COPY --from=overlay / /
# Fail container startup if an s6-rc initialization service fails.
ENV S6_BEHAVIOUR_IF_STAGE2_FAILS=2
ENTRYPOINT ["/init"]
CMD ["/bin/sh", "-c", "trap 'exit 0' TERM INT; i=0; while :; do i=$((i+1)); echo \"container-mutator demo tick $i\"; sleep 1; done"]
