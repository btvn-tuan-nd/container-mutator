#!/bin/sh
set -eu
if [ "$(cat /app/healthy)" = crash ]; then
    echo "counter: intentional startup failure" >&2
    exit 42
fi
number=$(cat /app/number)
printf '%s\n' "$number" > /run/counter-value
trap 'rm -f /run/counter-value; exit 0' TERM INT
while :; do
    printf 'counter=%s pid=%s\n' "$number" "$$"
    sleep 1
done
