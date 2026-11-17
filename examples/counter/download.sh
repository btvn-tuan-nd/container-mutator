#!/bin/sh
# Download and verify the three archives used by the service Dockerfile.
set -eu
case "${PLATFORM:-$(uname -m)}" in
    linux/amd64|x86_64|amd64)
        arch=amd64
        s6_arch=x86_64
        s6_sha=e6befcc96a437a3831386ecfc51808c5d3e939dc5fe3c02ae9284599e8aa2408
        mutator_sha=47abb2ae4c7ab7402b5507f60e18f104e3140c8857d0d6f679453293db95b74f
        ;;
    linux/arm64|aarch64|arm64)
        arch=arm64
        s6_arch=aarch64
        s6_sha=b17f17a82e7a515c682a91edaf2ffdabb73f891981b6c1fd712115693a2f8b4c
        mutator_sha=df7ccbbff4a3d4234f5aa3be733a72edc96b82bf7a911d48ea7bcae29d4aaa9e
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
if ! verify "$mutator_sha" mutator.tar.gz >/dev/null 2>&1; then
    # The repository is private: authenticate downloads on the host.
    gh release download v0.1.0 --repo btvn-tuan-nd/container-mutator \
        --pattern "container-mutator-integration-linux-$arch.tar.gz" \
        --output mutator.tar.gz --clobber
fi
verify "$mutator_sha" mutator.tar.gz
