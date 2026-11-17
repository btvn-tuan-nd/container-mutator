# container-mutator

A small Rust updater for services already managed by **s6-overlay**. It replaces one application directory (`/app` by default) inside the running container, without a Docker socket. Switching causes brief downtime.

Mutator polls, extracts complete snapshots, and journals release switches. **s6 owns application commands, readiness, supervision, and shutdown.** Crane owns registry authentication, HTTPS, and digest verification. Failed or interrupted trials restore the committed release; databases and application data are not rolled back.

## Try it locally

The example downloads and verifies **v0.2.0** from the public release. This version adds the single-app preset, `install-service`, and `MUTATOR_APP_DIR`.

```sh
nix develop path:.                         # optional devshell: Rust, Docker, crane, gh

docker compose up -d --wait registry
./examples/counter/publish.sh runtime      # installs the single-app tarball
./examples/counter/publish.sh update
docker compose --profile update-demo up -d --pull always counter
NUMBER=2 ./examples/counter/publish.sh update
# After the update commits, this returns 2:
curl http://127.0.0.1:8080/

HEALTHY=no ./examples/counter/publish.sh update
# After the readiness timeout and rollback, this still returns 2:
curl http://127.0.0.1:8080/
```

The example runs nginx serving `/srv/site/index.html` on port 8080. Each snapshot contains only the static page; nginx and its configuration stay in the runtime image. It installs s6-overlay, then mutator, by extracting their release tarballs into `/`. `publish.sh runtime` runs [download.sh](examples/counter/download.sh) to download and verify s6 and mutator first (or use a trusted local `MUTATOR_ARCHIVE`). The service Dockerfile sets `MUTATOR_COMMAND` and `MUTATOR_CHECK`; the single-app tarball supplies the s6 wiring.

To build and run locally without publishing:

```sh
./examples/counter/download.sh
docker build --target service -t mutator-counter:local examples/counter
docker run --rm -p 127.0.0.1:8080:8080 mutator-counter:local
```

Open `http://127.0.0.1:8080/` to read the baked-in counter value, with updates disabled. The Compose flow above enables registry polling. Published releases currently provide AMD64 assets only. ARM64 can still be built from source with `--platform linux/arm64` and supplied through `MUTATOR_ARCHIVE`.

`NUMBER=3 HEALTHY=no ./examples/counter/publish.sh update` publishes a snapshot without an index page, exercising HTTP readiness failure and rollback. The registry on `localhost:5000` is unauthenticated HTTP, for local testing only.

## Demo with GitHub Container Registry

Run the **GHCR counter demo** workflow from the Actions tab, or:

```sh
gh workflow run counter-demo.yml
```

It uses the workflow's registry credentials to publish private images at `ghcr.io/<repository-owner>/container-mutator-counter`. It starts counter 1, updates to 2, rejects an unhealthy 3 and restores 2, then updates to 4—all within the same container. The Actions summary records the result. Test containers are removed; the `runtime` and `live` images remain in GHCR. No personal registry token is required for this workflow.

The workflow builds the current single-app tarball first. The demo checks the value over HTTP. The workflow runs on GitHub's runner. Pulling these private images on your own machine additionally requires GHCR read access.

## Install: s6-overlay, then mutator

Download the s6-overlay noarch and architecture tarballs, plus the matching mutator single-app tarball from [releases](https://github.com/btvn-tuan-nd/container-mutator/releases). Verify their checksums before installing. The [example download script](examples/counter/download.sh) downloads and verifies the published AMD64 assets. Set `MUTATOR_VERSION` to select a release, or `MUTATOR_ARCHIVE` to use a trusted local mutator archive.

Extract all three into the image root. Docker automatically extracts **local** tarballs with `ADD`:

```dockerfile
FROM alpine:3.22
RUN apk add --no-cache ca-certificates nginx

ADD s6-overlay-noarch.tar.xz /
ADD s6-overlay-x86_64.tar.xz /
ADD container-mutator-single-app-linux-amd64.tar.gz /

COPY app/ /srv/site/
COPY nginx.conf /etc/nginx/nginx.conf
ENV MUTATOR_APP_DIR=/srv/site
ENV MUTATOR_RUNTIME=my-base-v1 S6_BEHAVIOUR_IF_STAGE2_FAILS=2
ENV MUTATOR_COMMAND="exec /usr/sbin/nginx -g 'daemon off;'" \
    MUTATOR_CHECK="wget -q -O /dev/null http://127.0.0.1:8080/"
EXPOSE 8080
ENTRYPOINT ["/init"]
CMD []
```

If s6-overlay is already installed, add just the mutator tarball. The equivalent manual extraction into an image root is:

```sh
tar -C / -Jxpf s6-overlay-noarch.tar.xz
tar -C / -Jxpf s6-overlay-x86_64.tar.xz
tar -C / -xzpf container-mutator-single-app-linux-amd64.tar.gz
```

Both mutator tarballs contain the binary, crane, and prepare/watch service definitions. The **single-app** tarball additionally supplies one `mutator-app` service and its bundle wiring. The **integration** tarball leaves application definitions to you, for existing s6 setups or multiple services. Install one variant. Both use the same service templates as `install-service`. The cleanup script targets s6-overlay 3.2.3.2, which the example pins. ARM64 release assets are currently omitted; source builds retain ARM64 support.

`nginx.conf` should serve `/srv/site` on port 8080, as in the [counter config](examples/counter/nginx.conf). Set `MUTATOR_IMAGE` when running the container to enable updates; leave it unset to run the baked-in application. Keep `/init` as the entrypoint.

The repository is public, so Docker can also download the tarball directly. Remote `ADD` does **not** extract archives; extract it explicitly (and pin the release checksum with `ADD --checksum=sha256:...` when building production images):

```dockerfile
ADD https://github.com/btvn-tuan-nd/container-mutator/releases/download/v0.2.0/container-mutator-single-app-linux-amd64.tar.gz /tmp/mutator.tar.gz
RUN tar -C / -xzpf /tmp/mutator.tar.gz && rm /tmp/mutator.tar.gz
```

The nginx example uses runtime ID `counter-nginx-alpine3.22-v1`. Rebuild and recreate an existing shell-counter container once before using these snapshots.

The root Dockerfile builds the release artifacts from source. Consumer images only need the tarballs. To test local changes instead of a published release:

```sh
docker build --platform linux/amd64 --target artifact --output type=local,dest=dist/amd64 .
(cd dist/amd64 && sha256sum -c SHA256SUMS)
export MUTATOR_ARCHIVE="$PWD/dist/amd64/container-mutator-single-app-linux-amd64.tar.gz"
```

### Application service contract

Mutator needs three things: a foreground command, a readiness signal, and complete cleanup before switching files. The single-app preset handles the s6 wiring:

```dockerfile
ENV MUTATOR_COMMAND="exec node --import tsx dist/main.js" \
    MUTATOR_CHECK="curl -fsS http://127.0.0.1:18020/sonya-gateway/healthz >/dev/null"
ENTRYPOINT ["/init"]
CMD []
```

These values use `/bin/sh` syntax. Use `exec` for the foreground command so it receives signals directly. Clearing an inherited `CMD` prevents s6 from also launching the base image's command.

For longer commands, copy executable scripts to `/etc/mutator/run` and `/etc/mutator/check`; each script takes precedence over its environment setting. Both run from `MUTATOR_APP_DIR`. Missing commands/checks or non-executable scripts fail `mutator-prepare`, including when updates are disabled. Set `S6_BEHAVIOUR_IF_STAGE2_FAILS=2` to exit the container on that failure.

Optional `/etc/mutator/cleanup` (or `MUTATOR_CLEANUP`) runs after process-group cleanup, from the service directory. It must be idempotent; failure is retried and blocks switching. Keep these scripts outside the replaceable directory. Readiness tools must be installed in the runtime image.

For multiple services, extract the **integration** tarball and use the build-time helper. It installs the same templates with per-service command scripts:

```dockerfile
RUN /opt/mutator/bin/mutator install-service --name gateway \
    --check 'curl -fsS http://127.0.0.1:18020/healthz >/dev/null' \
    -- node --import tsx dist/main.js
```

The command and check run with `MUTATOR_APP_DIR` as their working directory. Command arguments after `--` are preserved literally; `--check` is an explicit `/bin/sh` command. Install the tools your check uses. Repeat with another name for multiple services; all join one mutable bundle. An existing service definition is never overwritten. Use this helper during image builds, before `/init` starts.

The preset and helper supply `notification-fd=3`, `s6-notifyoncheck`, dependencies on prepare/base, `mutable-app` membership in `user2`, and process-group cleanup with the trial marker. Each check has a 5-second attempt timeout and 30-second overall readiness timeout. These services receive TERM, then KILL after 5 seconds if needed. Mutator's transition timeout also bounds update attempts.

For detached processes, add `--cleanup '/opt/gateway/cleanup-detached'`. This shell command runs after process-group cleanup, in the service directory, and must be idempotent. A nonzero exit is retried once per second; until cleanup succeeds, s6 does not report down-ready and mutator cannot switch releases. Put the cleanup program outside the replaceable directory. Services with more specialized supervision can still supply their own definitions instead of using the helper.

For manual definitions, preserve these rules:

- Mutable services are longruns in `mutable-app`, started through `user2`, depending on `mutator-prepare` and declaring readiness on fd 3.
- Enter the configured application directory on each launch; keep commands and s6 definitions outside it.
- Finish cleanup completely, then exit 125 while `/run/mutator/trial` exists. A failed trial must stay down for rollback.
- Include all dependents of mutable services in the bundle; mutator refuses to stop unrelated services.

Keep `/init` as entrypoint, without Docker `--init` or Compose `init: true`. Docker CMD alone is not the mutable service. Run the supervisor/updater with permission to replace the application directory; use an explicit privilege-dropping command when the application should run as a different user.

### Choosing the application directory

Set `MUTATOR_APP_DIR` in the image, for example `/srv/site` or `/opt/gateway`; the default is `/app`. It must initially be a real directory. Its parent and storage parents must not be symlink aliases. Both the application and release storage must be on the container root mount. Mounts at, inside, or above either tree (except `/` itself) are rejected: even a bind mount on the same device can prevent atomic renames.

Snapshots **always contain `app/`**, regardless of installation location. For `/srv/site`, the archive entry `app/index.html` becomes `/srv/site/index.html`. Absolute symlinks and application configuration are not rewritten; they must already match the destination.

The configured directory is recorded before bootstrap begins and in the release journal. Changing it on an initialized container is rejected; recreate the container to change it. Old journals without a directory field are treated as `/app`. Once new state is written, older mutator binaries may reject it; downgrade by recreating the container.

The selected path cannot overlap system trees (`/usr`, `/etc`, `/bin`, `/sbin`, `/lib`, `/lib64`, `/dev`, `/proc`, `/sys`, `/run`, `/nix`), `/init`, s6 (`/package`, `/command`), or mutator (`/opt/mutator`, `/var/lib/mutator`). Parents of those paths are also forbidden. Those paths remain fixed; persistent data and credentials belong outside the application tree.

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
| `MUTATOR_APP_DIR` | `/app`; application directory to replace |
| `MUTATOR_RUNTIME` | Required compatibility ID when enabled |
| `MUTATOR_POLL_INTERVAL` | `60` seconds |
| `MUTATOR_TIMEOUT` | `30` seconds per native transition, including finish waiting on stop |
| `MUTATOR_MAX_BYTES` | `4294967296`; compressed and expanded archive limit |
| `MUTATOR_MAX_ENTRIES` | `200000` archive entries |
| `DOCKER_CONFIG` | Standard crane/Docker credential directory, outside the application tree |
| `MUTATOR_INSECURE_REGISTRY` | `false`; passes crane `--insecure`, including disabling TLS verification |

Crane inherits the watch service environment, including credential helpers, proxy settings, and `SSL_CERT_FILE`. Mutator implements no separate authentication or HTTPS policy. Set native s6 startup/readiness/shutdown timeouts for the application; database-lock waits can also wait for another s6 transition.

## Boundaries

- Only `MUTATOR_APP_DIR` changes. OS libraries, mutator, crane, s6, service definitions, and image configuration require rebuilding/recreating the container.
- Updates require a writable root, normally root USER, and an initial real application directory. `/var/lib/mutator` and the application must be on the root mount. **No separate mounts above, at, or below either path**; persistent data belongs elsewhere.
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
