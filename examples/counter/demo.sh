#!/bin/sh
# Requires authenticated Docker registry access, MUTATOR_ARCHIVE, jq, and Docker.
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
: "${REGISTRY:?Set REGISTRY, e.g. ghcr.io/your-account}"
export REGISTRY
export PLATFORM=linux/amd64
repo="$REGISTRY/container-mutator-counter"
tmp=$(mktemp -d)
container=
cleanup() {
    if [ -n "$container" ]; then
        docker logs "$container" || :
        docker rm -f "$container" >/dev/null || :
    fi
    rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

NUMBER=1 HEALTHY=yes "$script_dir/publish.sh" runtime
# Reuse the verified crane distributed in the mutator archive.
tar -xzf "$script_dir/downloads/amd64/mutator.tar.gz" -C "$tmp" ./opt/mutator/bin/crane
export CRANE="$tmp/opt/mutator/bin/crane"
NUMBER=1 HEALTHY=yes "$script_dir/publish.sh" update

# Docker and crane use the same registry credentials, outside /app.
credentials=${DOCKER_CONFIG:-$HOME/.docker}
container=$(docker run -d \
    --mount "type=bind,src=$credentials,dst=/registry-auth,readonly" \
    -e DOCKER_CONFIG=/registry-auth \
    -e "MUTATOR_IMAGE=$repo:live" \
    -e MUTATOR_POLL_INTERVAL=2 -e MUTATOR_TIMEOUT=6 \
    "$repo:runtime")
started=$(docker inspect -f '{{.State.StartedAt}}' "$container")

wait_for() {
    field=$1 digest=$2 number=$3
    for attempt in $(seq 1 90); do
        state=$(docker exec "$container" cat /var/lib/mutator/selection.json 2>/dev/null || :)
        value=$(docker exec "$container" wget -q -O - http://127.0.0.1:8080/ 2>/dev/null || :)
        if [ "$value" = "$number" ] && printf '%s' "$state" | \
            jq -e --arg field "$field" --arg digest "$digest" \
            '.[$field] == $digest and .pending == null' >/dev/null; then
            printf 'PASS: %s %s; HTTP counter=%s\n' "$field" "$digest" "$value"
            return 0
        fi
        sleep 1
    done
    echo "Timed out waiting for $field=$digest, counter=$number" >&2
    return 1
}

wait_for digest "$("$CRANE" digest "$repo:live")" 1
NUMBER=2 HEALTHY=yes "$script_dir/publish.sh" update
wait_for digest "$("$CRANE" digest "$repo:live")" 2
NUMBER=3 HEALTHY=no "$script_dir/publish.sh" update
wait_for rejected "$("$CRANE" digest "$repo:live")" 2
NUMBER=4 HEALTHY=yes "$script_dir/publish.sh" update
wait_for digest "$("$CRANE" digest "$repo:live")" 4

test "$(docker exec "$container" sh -c 'printf "%s" "$MUTATOR_APP_DIR"')" = /srv/site
test "$(docker exec "$container" cat /var/lib/mutator/selection.json | jq -r .app_dir)" = /srv/site
test "$(docker inspect -f '{{.State.StartedAt}}' "$container")" = "$started"
test "$(docker inspect -f '{{.RestartCount}}' "$container")" = 0
printf 'PASS: container %s stayed running through updates and rollback\n' "$container"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    cat >> "$GITHUB_STEP_SUMMARY" <<SUMMARY
## GHCR counter demo passed

- Registry image: \`$repo\`
- Initial snapshot: counter 1
- Successful update: counter 2
- Unhealthy snapshot: counter 3 rejected, counter 2 restored
- Next successful update: counter 4
- Same container throughout; restart count 0
- Runtime tag: \`runtime\`; watched snapshot tag: \`live\`
- Temporary test container removed after the demo; images remain in GHCR.
SUMMARY
fi
