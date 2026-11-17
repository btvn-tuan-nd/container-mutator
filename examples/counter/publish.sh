#!/bin/sh
# Local demo: publish.sh runtime, then NUMBER=2 publish.sh update.
set -eu

runtime_id=counter-alpine3.22-v1
crane=${CRANE:-crane}
repo="${REGISTRY:-localhost:5000}/container-mutator-counter"
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
command -v "$crane" >/dev/null || {
    echo "Enter the Nix devshell or set CRANE to the crane executable" >&2
    exit 1
}
case "${PLATFORM:-$(uname -m)}" in
    linux/amd64|x86_64|amd64) platform=linux/amd64 ;;
    linux/arm64|aarch64|arm64) platform=linux/arm64 ;;
    *) echo "Unsupported platform; use linux/amd64 or linux/arm64" >&2; exit 1 ;;
esac
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM

build_app() {
    docker buildx build --platform "$platform" \
        --build-arg "NUMBER=${NUMBER:-1}" --build-arg "HEALTHY=${HEALTHY:-yes}" \
        --build-arg "RUNTIME_ID=$runtime_id" \
        "$@" "$script_dir"
}

case "${1:-}" in
runtime)
    build_app --target service --tag "$repo:service" --push
    service=$("$crane" digest --full-ref --platform "$platform" "$repo:service")
    docker buildx build --platform "$platform" --target artifact \
        --output "type=local,dest=$tmp/overlay" "$repo_root"
    # The service image already has s6, /init, the native counter service, and
    # MUTATOR_RUNTIME. Only append the integration layer; keep the entrypoint.
    "$crane" mutate "$service" \
        --append "$tmp/overlay/container-mutator-integration-linux-${platform#linux/}.tar.gz" \
        --tag "$repo:runtime"
    ;;
update)
    build_app --target payload --output "type=local,dest=$tmp/payload"
    # Never expose an unlabelled intermediate image on the watched tag.
    snapshot=$("$crane" append --oci-empty-base \
        --new_layer "$tmp/payload/app.tar" --new_tag "$repo:snapshot-staging")
    "$crane" mutate "$snapshot" \
        --label "io.container-mutator.format=1" \
        --label "io.container-mutator.runtime=$runtime_id" \
        --set-platform "$platform" --tag "$repo:live"
    ;;
*)
    echo "usage: $0 runtime|update" >&2
    exit 2
    ;;
esac
