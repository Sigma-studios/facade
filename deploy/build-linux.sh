#!/usr/bin/env bash
# Builds static (musl) Linux binaries in Docker, so they run on any distribution.
#
#   deploy/build-linux.sh [amd64|arm64]     default amd64
#
# Output: dist/<arch>/{facaded,facade} plus the deploy files, ready to scp to a VPS.
set -euo pipefail

arch=${1:-amd64}
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/dist/$arch
mkdir -p "$out"

docker run --rm --platform "linux/$arch" \
    -v "$root":/src -w /src \
    -v facade-cargo-registry:/usr/local/cargo/registry \
    -v "facade-target-$arch":/src/target \
    -v "$out":/out \
    rust:1-alpine sh -c '
        apk add --no-cache musl-dev >/dev/null &&
        cargo build --release --bin facaded --bin facade &&
        cp target/release/facaded target/release/facade /out/'

cp "$root"/deploy/install.sh "$root"/deploy/*.service "$out"/
echo "built $out:"
ls -lh "$out"
echo
echo "copy to the server and install:"
echo "  scp -r $out root@<vps>:/root/facade && ssh root@<vps> /root/facade/install.sh server"
