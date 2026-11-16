# syntax=docker/dockerfile:1
#
# container-mutator: s6-overlay base plus the statically linked mutator executable.
#
# Optional pinned development tooling (Nix with nix-command and flakes enabled):
#   nix develop path:.
#   cargo test --locked
#   cargo clippy --locked --all-targets -- -D warnings
#   cargo fmt --check
# path:. includes untracked files; use . once tracked in Git.
# flake.lock pins development tools only. Docker builds do not require Nix.
# Compose builds current source directly; exporting an archive is optional.
#
# Targets:
#   final    (default) Alpine convenience image; `docker compose up -d --build mutator`.
#   overlay  scratch image containing only the installed s6/mutator files, usable
#            with COPY --from=<image> / /.
#   artifact BuildKit local export with the installable tarball and SHA256SUMS.
#
# Export an installable archive (separate output directories per platform):
#   docker buildx build --platform linux/amd64 --target artifact --output type=local,dest=dist/amd64 .
#   docker buildx build --platform linux/arm64 --target artifact --output type=local,dest=dist/arm64 .
# Install into an Alpine or Debian root (the archive has no distro base files):
#   ( cd dist/amd64 && sha256sum -c SHA256SUMS )
#   tar -C / -xzf dist/amd64/container-mutator-linux-amd64.tar.gz
# Then run it as PID 1 with no extra init:
#   ENTRYPOINT ["/opt/mutator/bin/mutator", "boot", "--"]
#   CMD ["your", "foreground", "command", "arg"]
#
# boot execs /init after saving argv/cwd/environment in /run/mutator/boot.json;
# This private, versioned state is regenerated on boot, not a configuration API.
# /run must be writable before boot and allow execution for s6's runtime files.
# `boot` must be PID 1 (no Docker --init). s6 supervises mutator-app, which runs
# `mutator launch` and execs the saved command. Applications must be foreground,
# noninteractive, and keep stdout/stderr inherited. A normal app exit is restarted
# by s6; container shutdown is bounded by mutator-app/timeout-kill and
# flag-timeout-killpg (SIGKILL to the app's process group after 5 s).
# For a read-only root: -e S6_READ_ONLY_ROOT=1 --tmpfs /run:exec
# For a non-root USER, /run must also be writable by that UID/GID.
# Re-state an inherited CMD after replacing ENTRYPOINT; Docker otherwise clears it.
#
# Licenses: the pinned s6-overlay release tarballs carry no license files. Verbatim
# upstream COPYING notices for every bundled project are installed at
# /opt/mutator/share/licenses/<project>-<version>/COPYING:
#   s6-overlay-3.2.3.2, s6-overlay-helpers-0.1.2.2
#       just-containers/<project> tag v<version>
#   skalibs-2.15.1.0, execline-2.9.9.2, s6-2.15.1.0, s6-rc-0.7.0.0,
#   s6-linux-init-1.2.0.2, s6-linux-utils-2.6.4.1,
#   s6-portable-utils-2.3.1.2, s6-networking-2.8.0.1, s6-dns-2.4.1.3
#       skarnet/<project> tag v<version>
# Source URL: https://raw.githubusercontent.com/<owner>/<project>/<tag>/COPYING

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

# Assemble into an empty root so the overlay contains no Alpine base files.
FROM alpine:3.22 AS rootfs
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
COPY overlay/rootfs/ /rootfs/
COPY --from=build /out/mutator /rootfs/opt/mutator/bin/mutator
# Normalize only mutator-owned paths, preserving upstream s6 metadata.
RUN set -eux; \
    chmod 0755 \
      /rootfs/etc/s6-overlay/s6-rc.d/mutator-app \
      /rootfs/etc/s6-overlay/s6-rc.d/mutator-app/dependencies.d \
      /rootfs/opt \
      /rootfs/opt/mutator \
      /rootfs/opt/mutator/bin \
      /rootfs/opt/mutator/share \
      /rootfs/etc/s6-overlay/s6-rc.d/mutator-app/run \
      /rootfs/opt/mutator/bin/mutator; \
    find /rootfs/opt/mutator/share/licenses -type d -exec chmod 0755 {} \; ; \
    chmod 0644 \
      /rootfs/etc/s6-overlay/s6-rc.d/mutator-app/type \
      /rootfs/etc/s6-overlay/s6-rc.d/mutator-app/timeout-kill \
      /rootfs/etc/s6-overlay/s6-rc.d/mutator-app/flag-timeout-killpg \
      /rootfs/etc/s6-overlay/s6-rc.d/mutator-app/dependencies.d/base \
      /rootfs/etc/s6-overlay/user-bundles.d/user/contents.d/mutator-app; \
    find /rootfs/opt/mutator/share/licenses -type f -exec chmod 0644 {} \;

FROM scratch AS overlay
COPY --from=rootfs /rootfs/ /

FROM alpine:3.22 AS archive
ARG TARGETARCH
COPY --from=overlay / /overlay
RUN set -eux; \
    mkdir -p /out; \
    tarball="container-mutator-linux-${TARGETARCH}.tar.gz"; \
    tar -C /overlay -czf "/out/${tarball}" .; \
    cd /out; \
    sha256sum "$tarball" > SHA256SUMS

FROM scratch AS artifact
COPY --from=archive /out/ /

FROM alpine:3.22 AS final
COPY --from=overlay / /
# Fail container startup if an s6-rc initialization service fails.
ENV S6_BEHAVIOUR_IF_STAGE2_FAILS=2
# boot captures this CMD, then execs /init; mutator-app replays CMD under s6.
ENTRYPOINT ["/opt/mutator/bin/mutator", "boot", "--"]
CMD ["/bin/sh", "-c", "trap 'exit 0' TERM INT; i=0; while :; do i=$((i+1)); echo \"container-mutator demo tick $i\"; sleep 1; done"]
