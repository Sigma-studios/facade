#!/usr/bin/env bash
# Updates facade on a server in place, from a release folder (deploy/build-linux.sh).
# Servers can also update themselves from the builds CI publishes (Settings →
# Updates, `facade server update`); this is for builds of your own.
#
#   deploy/upgrade.sh <ssh target> [--dist dist/amd64] [--yes]
#
# 1. Asks the new binary which running apps it would restart or rebuild, and
#    waits for a yes (--yes skips the question).
# 2. Backs up the server's database and key, and keeps the previous binaries as
#    facaded.prev / facade.prev.
# 3. Installs, restarts facaded and waits until it is healthy (its own agent
#    connected). If it is not within 90 s, the previous binary goes back.
# 4. Hands the entry point (facaded-ingress) over to the new binary: open
#    connections, WebSockets included, finish on the old process.
#
# Apps keep running throughout: they belong to Docker. Other nodes update their
# agent themselves when it reconnects to the new server.
#
# The ssh user must be root or allowed to sudo without a password.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
target=""
dist="$here/../dist/amd64"
yes=no
while [[ $# -gt 0 ]]; do
    case $1 in
        --dist) dist=${2:?--dist needs a folder}; shift 2 ;;
        --yes | -y) yes=yes; shift ;;
        -*) echo "unknown option $1" >&2; exit 2 ;;
        *) target=$1; shift ;;
    esac
done
[[ -n $target ]] || { echo "usage: $0 <ssh target> [--dist DIR] [--yes]" >&2; exit 2; }
for file in facaded facade facaded.service facaded-ingress.service facaded-agent.service; do
    [[ -f $dist/$file ]] || { echo "no $dist/$file (run deploy/build-linux.sh)" >&2; exit 1; }
done

stage=$(ssh "$target" 'mktemp -d /tmp/facade-upgrade.XXXXXX')
trap 'ssh "$target" "rm -rf $stage" >/dev/null 2>&1 || true' EXIT
echo "uploading $(du -sh "$dist/facaded" | cut -f1) to $target…"
scp -q "$dist"/facaded "$dist"/facade "$dist"/*.service "$target:$stage/"

echo
echo "what the new version does to the apps running on this server:"
ssh "$target" "sudo $stage/facaded upgrade-check -c /etc/facade/server.toml"
echo
if [[ $yes != yes ]]; then
    read -r -p "Install it? [y/N] " answer
    [[ $answer == y || $answer == Y || $answer == yes ]] || { echo "aborted"; exit 1; }
fi

# Backs up, installs, restarts, waits for health (rolling back if needed) and
# hands the entry point over: the same steps as an update from the web UI.
ssh "$target" "sudo $stage/facaded apply-update $stage --config /etc/facade/server.toml"
