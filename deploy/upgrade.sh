#!/usr/bin/env bash
# Updates facade on a server in place, from a release folder (deploy/build-linux.sh).
#
#   deploy/upgrade.sh <ssh target> [--dist dist/amd64] [--yes]
#
# 1. Asks the new binary which running apps it would restart or rebuild, and
#    waits for a yes (--yes skips the question).
# 2. Backs up the server's database and key, and keeps the previous binaries as
#    facaded.prev / facade.prev.
# 3. Installs, restarts facaded and waits until it is healthy (its own agent
#    connected). If it is not within 60 s, the previous binary goes back.
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

ssh "$target" "sudo bash -s -- $stage" <<'REMOTE'
set -euo pipefail
stage=$1
bin=/usr/local/bin
config=/etc/facade/server.toml

echo "backing up the database and key: $("$stage/facaded" backup-state -c "$config")"
cp -p "$bin/facaded" "$bin/facaded.prev"
[[ -f $bin/facade ]] && cp -p "$bin/facade" "$bin/facade.prev"

# Atomic replacement: the path always holds a complete binary.
put() { cp "$1" "$2.new" && chmod 0755 "$2.new" && mv -f "$2.new" "$2"; }
put "$stage/facaded" "$bin/facaded"
put "$stage/facade" "$bin/facade"
# Units that are installed get their new version.
for unit in "$stage"/*.service; do
    name=$(basename "$unit")
    [[ -f /etc/systemd/system/$name ]] && install -m 0644 "$unit" "/etc/systemd/system/$name"
done
systemctl daemon-reload

port=$(sed -n 's/^api_listen *= *"[^"]*:\([0-9]*\)".*/\1/p' "$config")
port=${port:-7701}
systemctl restart facaded
healthy=no
for _ in $(seq 1 60); do
    if [[ $(curl -sk -o /dev/null -w '%{http_code}' "https://127.0.0.1:$port/api/health" || true) == 200 ]]; then
        healthy=yes
        break
    fi
    sleep 1
done
if [[ $healthy != yes ]]; then
    echo "facaded is not healthy after the update; going back to the previous version:" >&2
    journalctl -u facaded -n 30 --no-pager >&2 || true
    mv -f "$bin/facaded.prev" "$bin/facaded"
    [[ -f $bin/facade.prev ]] && mv -f "$bin/facade.prev" "$bin/facade"
    systemctl restart facaded
    exit 1
fi
echo "facaded is up"

if systemctl is-active -q facaded-ingress; then
    before=$(systemctl show facaded-ingress -p MainPID --value)
    systemctl reload facaded-ingress
    after=$before
    for _ in $(seq 1 40); do
        after=$(systemctl show facaded-ingress -p MainPID --value)
        [[ $after != "$before" ]] && break
        sleep 0.5
    done
    if [[ $after == "$before" ]]; then
        echo "warning: the entry point kept the previous version (it still serves);" >&2
        echo "         see journalctl -u facaded-ingress" >&2
    else
        echo "entry point handed over to the new version (pid $before → $after)"
    fi
fi
echo "updated; the previous binary is $bin/facaded.prev"
REMOTE
