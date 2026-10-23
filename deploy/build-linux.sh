#!/usr/bin/env bash
# Builds static (musl) Linux binaries in Docker, so they run on any distribution.
# Needs trunk and the wasm32-unknown-unknown target locally for the web UI.
#
#   deploy/build-linux.sh [amd64|arm64] [--skip-ui]     default amd64
#
# --skip-ui: embed the web UI already in crates/facade-ui/dist (CI builds it once).
# The binaries carry the commit they were built from (`facaded --version`, and
# Settings → Updates) when the checkout has no uncommitted changes, or the one in
# $FACADE_COMMIT. $FACADE_BUILD_CACHE: a folder for the cargo registry and target
# instead of Docker volumes.
#
# Output: dist/<arch>/{facaded,facade} plus the deploy files, ready to scp to a VPS.
set -euo pipefail

arch=amd64
skip_ui=no
for arg in "$@"; do
    case $arg in
        --skip-ui) skip_ui=yes ;;
        amd64 | arm64) arch=$arg ;;
        *) echo "usage: $0 [amd64|arm64] [--skip-ui]" >&2; exit 2 ;;
    esac
done
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/dist/$arch
mkdir -p "$out"

if [[ -z ${FACADE_COMMIT:-} ]] && git -C "$root" diff --quiet HEAD 2>/dev/null; then
    FACADE_COMMIT=$(git -C "$root" rev-parse HEAD)
fi

# The web UI is WebAssembly, so it is built once here and embedded into facaded.
if [[ $skip_ui == no ]]; then
    (cd "$root/crates/facade-ui" && trunk build --release)
fi

if [[ -n ${FACADE_BUILD_CACHE:-} ]]; then
    mkdir -p "$FACADE_BUILD_CACHE/registry" "$FACADE_BUILD_CACHE/target"
    registry=$FACADE_BUILD_CACHE/registry
    target=$FACADE_BUILD_CACHE/target
else
    registry=facade-cargo-registry
    target=facade-target-$arch
fi

docker run --rm --platform "linux/$arch" \
    -v "$root":/src -w /src \
    -v "$registry":/usr/local/cargo/registry \
    -v "$target":/src/target \
    -v "$out":/out \
    -e FACADE_COMMIT="${FACADE_COMMIT:-}" \
    docker.io/library/rust:1-alpine sh -c '
        apk add --no-cache musl-dev >/dev/null &&
        cargo build --release --bin facaded --bin facade &&
        cp target/release/facaded target/release/facade /out/'

cp "$root"/deploy/install.sh "$root"/deploy/*.service "$out"/
echo "built $out${FACADE_COMMIT:+ at ${FACADE_COMMIT:0:7}}:"
ls -lh "$out"
echo
echo "copy to the server and install:"
echo "  scp -r $out root@<vps>:/root/facade && ssh root@<vps> /root/facade/install.sh server"
