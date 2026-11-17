# container-mutator

A small Rust updater for services already managed by **s6-overlay**. It replaces `/app` inside the running container, without a Docker socket. Switching causes brief downtime.

Mutator polls, extracts complete snapshots, and journals release switches. **s6 owns application commands, readiness, supervision, and shutdown.** Crane owns registry authentication, HTTPS, and digest verification. Failed or interrupted trials restore the committed release; databases and application data are not rolled back.

## Try it locally

```sh
nix develop path:.                         # optional devshell: Rust, Docker, crane

docker compose up -d --wait registry
./examples/counter/publish.sh runtime      # s6 service image + mutator integration
./examples/counter/publish.sh update
docker compose --profile update-demo up -d --pull always counter
NUMBER=2 ./examples/counter/publish.sh update
docker compose logs -f counter
```

`NUMBER=3 HEALTHY=no` exercises readiness rollback; `HEALTHY=crash` exercises startup-crash rejection. The registry on `localhost:5000` is unauthenticated HTTP, for local testing only.

## Install into an s6 image

```sh
docker buildx build --platform linux/amd64 --target artifact \
  --output type=local,dest=dist/amd64 .
(cd dist/amd64 && sha256sum -c SHA256SUMS)
```

Or download a prebuilt tarball from [releases](https://github.com/OWNER/container-mutator/releases) (adjust `OWNER` to this repository's location; requires s6-overlay ≥ 3.2.1.0, which introduced `user-bundles.d` — Alpine's `s6-overlay` package is currently older, so install from the official tarballs), verify it, and install it like the local build:

```sh
wget https://github.com/OWNER/container-mutator/releases/download/v0.1.0/container-mutator-integration-linux-amd64.tar.gz
echo "<sha256 from SHA256SUMS-amd64>  container-mutator-integration-linux-amd64.tar.gz" | sha256sum -c -
```

The archive installs mutator, pinned crane, and two native s6 service definitions. **It does not contain s6 or `/init`.** Use the matching `arm64` build for ARM64.

For an existing s6-overlay image:

```dockerfile
ADD container-mutator-integration-linux-amd64.tar.gz /
ENV MUTATOR_RUNTIME=my-base-v1 S6_BEHAVIOUR_IF_STAGE2_FAILS=2
ENTRYPOINT ["/init"]
```

Alternatively, copy the raw binary to `/opt/mutator/bin/mutator`, install crane at `/opt/mutator/bin/crane`, and copy the reference definitions from `overlay/rootfs/etc/s6-overlay/`. The image supplies s6-overlay, `/bin/sh`, and CA certificates. The convenience `final` Docker target installs s6-overlay separately; the reference version is 3.2.3.2.

### Application service contract

Use [the counter definitions](examples/counter/rootfs/etc/s6-overlay) as a working example:

- `mutator-prepare` is a **oneshot**, running `mutator prepare` before mutable services start. It bootstraps `/app` and recovers interrupted updates.
- `mutator-watch` is a **longrun**, running `mutator watch`. It depends on prepare, **never on the services it updates**.
- Put application longruns in `user-bundles.d/mutable-app/contents.d/`. Start that bundle through `user-bundles.d/user2/contents.d/mutable-app`, **not `user`**. s6-overlay's legacy-services depends on `user`; including mutable services there would make updates stop unrelated services.
- Every mutable service depends on `mutator-prepare`, declares `notification-fd` **3**, and signals real readiness. Use native notification or `s6-notifyoncheck`; mutator has no healthcheck runner.
- Each service enters `/app` on every launch. With `s6-notifyoncheck`, retain the service working directory and use `/command/cd /app` for the application, as the counter does.
- Each `finish` script cleans up its descendants, then **exits 125 while `/run/mutator/trial` exists**, before any normal restart/container-halt policy. This prevents a crashing candidate from restarting into a successful trial. Configure cleanup so down-ready means cleanup really finished; the counter waits for its process group with `timeout-finish=0` and lets mutator bound the wait.
- The bundle must contain all services that depend on its members. Mutator rejects transitions that would stop services outside the bundle. Fixed dependencies can remain outside and keep running.

Keep commands and s6 definitions outside the replaceable tree. Install these definitions when building the base image; don't run `prepare` manually against live services. Leave entrypoint `/init`, without Docker `--init` or Compose `init: true`.

For the gateway, define one longrun and move logs/state out of `/app`. For the workspace, reuse its s6 installation, move updateable code into `/app`, and keep `volume-guard` outside the mutable bundle. Hub/kernel cleanup remains the workspace's responsibility.

## Publish updates

Each update is a **single-layer image containing the complete `app/` tree**, not a runnable service image:

```sh
tar -C release-root -cf app.tar app
snapshot=$(crane append --oci-empty-base \
  --new_layer app.tar --new_tag "$UPDATE_REPO:staging")
crane mutate "$snapshot" \
  --set-platform linux/amd64 \
  --label io.container-mutator.format=1 \
  --label io.container-mutator.runtime=my-base-v1 \
  --tag "$UPDATE_REPO:live"
```

Set `MUTATOR_IMAGE` to that watched snapshot tag. The runtime label must match `MUTATOR_RUNTIME`: this is the publisher's assertion of compatibility with the fixed base, **not automatic ABI validation**. Publish the watched tag only after attaching labels and platform. Crane assembles images; existing build tools still compile the application.

## Configuration

| Variable | Default / purpose |
| --- | --- |
| `MUTATOR_IMAGE` | Snapshot reference; unset disables updates |
| `MUTATOR_RUNTIME` | Required compatibility ID when enabled |
| `MUTATOR_POLL_INTERVAL` | `60` seconds |
| `MUTATOR_TIMEOUT` | `30` seconds per native transition, including finish waiting on stop |
| `MUTATOR_MAX_BYTES` | `4294967296`; compressed and expanded archive limit |
| `MUTATOR_MAX_ENTRIES` | `200000` archive entries |
| `DOCKER_CONFIG` | Standard crane/Docker credential directory, outside `/app` |
| `MUTATOR_INSECURE_REGISTRY` | `false`; passes crane `--insecure`, including disabling TLS verification |

Crane inherits the watch service environment, including credential helpers, proxy settings, and `SSL_CERT_FILE`. Mutator implements no separate authentication or HTTPS policy. Set native s6 startup/readiness/shutdown timeouts for the application; database-lock waits can also wait for another s6 transition.

## Boundaries

- Only `/app` changes. OS libraries, mutator, crane, s6, service definitions, and image configuration require rebuilding/recreating the container.
- Updates require a writable root, normally root USER, and an initial real `/app` directory. `/var/lib/mutator` must share `/app`'s parent filesystem. **No mounts at or below either path**; persistent data belongs elsewhere.
- Stops must finish before the switch. Services that create separate sessions need their own reliable cleanup; mutator does not sweep process trees.
- Updates survive container restart, not recreation. One previous release and the latest rejected digest are retained. Download failures are retried; failed/interrupted candidates are quarantined.
- Use trusted snapshots: digests prove integrity, not publisher identity. Archives cannot contain whiteouts, sparse/special files, duplicate paths, or entries outside `app/`. Extraction preserves rwx permissions, not ownership, capabilities, or xattrs; metadata is capped at 64 KiB per entry. Disk-space checks reserve 64 MiB headroom on a best-effort basis.

## Development

Nix is devshell-only. Docker builds do not require it.

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```
