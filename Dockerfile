# syntax=docker/dockerfile:1
# Build and run locally: docker compose up -d --build mutator
# Derived images inherit /init and the supervised mutator service.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release --locked

FROM alpine:3.22 AS s6
RUN apk add --no-cache curl xz
ARG TARGETARCH
ADD --checksum=sha256:5379750ed30a84bbd2e2dd74847ba6b5bd29cd0b2e3ea2ec58049b57eb2eda12 \
    https://github.com/just-containers/s6-overlay/releases/download/v3.2.3.2/s6-overlay-noarch.tar.xz /tmp/s6-overlay-noarch.tar.xz
RUN case "$TARGETARCH" in \
      amd64) arch=x86_64; checksum=e6befcc96a437a3831386ecfc51808c5d3e939dc5fe3c02ae9284599e8aa2408 ;; \
      arm64) arch=aarch64; checksum=b17f17a82e7a515c682a91edaf2ffdabb73f891981b6c1fd712115693a2f8b4c ;; \
      *) echo "Unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && curl --fail --location --show-error --silent \
      "https://github.com/just-containers/s6-overlay/releases/download/v3.2.3.2/s6-overlay-${arch}.tar.xz" \
      --output /tmp/s6-overlay-arch.tar.xz \
    && printf '%s  %s\n' "$checksum" /tmp/s6-overlay-arch.tar.xz | sha256sum -c - \
    && mkdir /rootfs \
    && tar -C /rootfs -Jxpf /tmp/s6-overlay-noarch.tar.xz \
    && tar -C /rootfs -Jxpf /tmp/s6-overlay-arch.tar.xz

FROM alpine:3.22
COPY --from=s6 /rootfs/ /
COPY --from=build /build/target/release/mutator /opt/mutator/bin/mutator
COPY overlay/rootfs/ /
RUN chmod 0755 /etc/s6-overlay/s6-rc.d/mutator/run
# Fail container startup if an s6-rc initialization service fails.
ENV S6_BEHAVIOUR_IF_STAGE2_FAILS=2
ENTRYPOINT ["/init"]
CMD []
