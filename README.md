# container-mutator

A small Rust updater for services already managed by **s6-overlay**. It replaces `/app` inside the running container, without a Docker socket. Switching causes brief downtime.

Mutator polls, extracts complete snapshots, and journals release switches. **s6 owns application commands, readiness, supervision, and shutdown.** Crane owns registry authentication, HTTPS, and digest verification. Failed or interrupted trials restore the committed release; databases and application data are not rolled back.

## Try it locally

```sh
nix develop path:.                         # optional devshell: Rust, Docker, crane, gh

docker compose up -d --wait registry
./examples/counter/publish.sh runtime      # installs the published mutator v0.1.0
./examples/counter/publish.sh update
docker compose --profile update-demo up -d --pull always counter
NUMBER=2 ./examples/counter/publish.sh update
docker compose logs -f counter
```

The example installs s6-overlay, then mutator, by extracting their release tarballs into `/`. `publish.sh runtime` runs [download.sh](examples/counter/download.sh) to download and verify the archives first. Since this repository is private, run `gh auth login` once. No Rust build is needed.

To build and run locally without publishing:

```sh
./examples/counter/download.sh
docker build --target service -t mutator-counter:local examples/counter
docker run --rm mutator-counter:local
```

This runs the baked-in counter with updates disabled. The Compose flow above enables registry polling. Set `PLATFORM=linux/arm64` when downloading for ARM64 and pass `--platform linux/arm64` to the build.

`NUMBER=3 HEALTHY=no` exercises readiness rollback; `HEALTHY=crash` exercises startup-crash rejection. The registry on `localhost:5000` is unauthenticated HTTP, for local testing only.

## Demo with GitHub Container Registry

Run the **GHCR counter demo** workflow from the Actions tab, or:

```sh
gh workflow run counter-demo.yml
```

It uses the workflow's registry credentials to publish private images at `ghcr.io/<repository-owner>/container-mutator-counter`. It starts counter 1, updates to 2, rejects an unhealthy 3 and restores 2, then updates to 4—all within the same container. The Actions summary records the result. Test containers are removed; the `runtime` and `live` images remain in GHCR. No personal registry token is required for this workflow.

The workflow runs on GitHub's runner. Pulling these private images on your own machine additionally requires GHCR read access.

## Install: s6-overlay, then mutator

Download the s6-overlay noarch and architecture tarballs, plus the matching mutator integration tarball from [releases](https://github.com/btvn-tuan-nd/container-mutator/releases). Verify their checksums before installing. The [example download script](examples/counter/download.sh) does this for AMD64 or ARM64.

Extract all three into the image root. Docker automatically extracts **local** tarballs with `ADD`:

```dockerfile
FROM alpine:3.22
RUN apk add --no-cache ca-certificates

ADD s6-overlay-noarch.tar.xz /
ADD s6-overlay-x86_64.tar.xz /
ADD container-mutator-integration-linux-amd64.tar.gz /

COPY app/ /app/
COPY rootfs/ /
ENV MUTATOR_RUNTIME=my-base-v1 S6_BEHAVIOUR_IF_STAGE2_FAILS=2
ENTRYPOINT ["/init"]
```

If s6-overlay is already installed, add just the mutator tarball. The equivalent manual extraction into an image root is:

```sh
tar -C / -Jxpf s6-overlay-noarch.tar.xz
tar -C / -Jxpf s6-overlay-x86_64.tar.xz
tar -C / -xzpf container-mutator-integration-linux-amd64.tar.gz
```

Mutator's tarball contains its binary, crane, and the prepare/watch service definitions. Use s6-overlay ≥ 3.2.1.0; the example pins 3.2.3.2. Use the `aarch64` s6 archive and `arm64` mutator archive for ARM64.

`rootfs/` supplies your application's s6 service definitions, as described below. Set `MUTATOR_IMAGE` when running the container to enable updates; leave it unset to run the baked-in application. Keep `/init` as the entrypoint.

For this private repository, download mutator with your authenticated GitHub CLI before building:

```sh
gh release download v0.1.0 --repo btvn-tuan-nd/container-mutator \
  --pattern 'container-mutator-integration-linux-amd64.tar.gz'
echo "47abb2ae4c7ab7402b5507f60e18f104e3140c8857d0d6f679453293db95b74f  container-mutator-integration-linux-amd64.tar.gz" | sha256sum -c -
```

The root Dockerfile builds the release artifacts from source. Consumer images only need the tarballs.

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
