#!/bin/sh
# Download verified s6 and mutator archives, or use a trusted local mutator archive.
set -eu
# A trusted local archive can override the published release for development.
archive=
if [ -n "${MUTATOR_ARCHIVE:-}" ]; then
    archive=$(realpath "$MUTATOR_ARCHIVE")
    test -f "$archive"
fi
case "${PLATFORM:-$(uname -m)}" in
    linux/amd64|x86_64|amd64)
        arch=amd64
        s6_arch=x86_64
        s6_sha=e6befcc96a437a3831386ecfc51808c5d3e939dc5fe3c02ae9284599e8aa2408
        ;;
    linux/arm64|aarch64|arm64)
        arch=arm64
        s6_arch=aarch64
        s6_sha=b17f17a82e7a515c682a91edaf2ffdabb73f891981b6c1fd712115693a2f8b4c
        ;;
    *) echo "Unsupported platform; use linux/amd64 or linux/arm64" >&2; exit 1 ;;
esac
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
mkdir -p "$script_dir/downloads/$arch"
cd "$script_dir/downloads/$arch"
verify() {
    printf '%s  %s\n' "$1" "$2" | sha256sum -c -
}
s6_url=https://github.com/just-containers/s6-overlay/releases/download/v3.2.3.2
for part in noarch arch; do
    if [ "$part" = noarch ]; then
        source=noarch
        checksum=5379750ed30a84bbd2e2dd74847ba6b5bd29cd0b2e3ea2ec58049b57eb2eda12
    else
        source=$s6_arch
        checksum=$s6_sha
    fi
    file="s6-overlay-$part.tar.xz"
    if ! verify "$checksum" "$file" >/dev/null 2>&1; then
        curl -fLsS "$s6_url/s6-overlay-$source.tar.xz" -o "$file"
    fi
    verify "$checksum" "$file"
done
if [ -n "$archive" ]; then
    cp "$archive" mutator.tar.gz
else
    version=${MUTATOR_VERSION:-0.2.0}
    release="https://github.com/btvn-tuan-nd/container-mutator/releases/download/v$version"
    file="container-mutator-single-app-linux-$arch.tar.gz"
    curl -fLsS "$release/SHA256SUMS-$arch" -o SHA256SUMS
    curl -fLsS "$release/$file" -o "$file"
    awk -v name="$file" '$2 == name' SHA256SUMS | sha256sum -c -
    cp "$file" mutator.tar.gz
fi
if ! tar -tzf mutator.tar.gz | grep -q '^\./etc/s6-overlay/s6-rc.d/mutator-app/data/preset$'; then
    echo 'Mutator archive must be the single-app tarball' >&2
    exit 1
fi
