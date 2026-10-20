#!/usr/bin/env bash
# Installs facade on an Ubuntu/Debian machine, as root.
#
#   install.sh server [public_host]   the control plane (also runs apps on this machine)
#   install.sh agent <join-token>     an extra node, token from `facade node add <name>`
#
# Expects the Linux `facaded` binary next to this script (see deploy/build-linux.sh),
# or its path in $FACADED.
set -euo pipefail

role=${1:-}
here=$(cd "$(dirname "$0")" && pwd)
binary=${FACADED:-$here/facaded}

die() { echo "error: $*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || die "run as root"
[[ -x $binary ]] || die "no facaded binary at $binary (set FACADED=/path/to/facaded)"
[[ $role == server || $role == agent ]] || die "usage: $0 server [public_host] | agent <join-token>"

if ! command -v docker >/dev/null; then
    echo "installing Docker…"
    curl -fsSL https://get.docker.com | sh
fi
if ! command -v git >/dev/null; then
    apt-get update -qq && apt-get install -y -qq git
fi

install -m 0755 "$binary" /usr/local/bin/facaded

open_ports() {
    if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
        for port in "$@"; do ufw allow "$port"; done
    fi
}

if [[ $role == server ]]; then
    public_host=${2:-$(hostname -I | awk '{print $1}')}
    mkdir -p /etc/facade
    if [[ ! -f /etc/facade/server.toml ]]; then
        cat > /etc/facade/server.toml <<EOF
public_host = "$public_host"

[acme]
# email = "you@example.com"
EOF
    fi
    install -m 0644 "$here/facaded.service" /etc/systemd/system/facaded.service
    systemctl daemon-reload
    systemctl enable --now facaded
    open_ports 80/tcp 443/tcp 7700/tcp 7701/tcp
    sleep 2
    echo
    echo "facade server running. From your computer:"
    echo "  facade login https://$public_host:7701"
    echo "with user admin and the password in /var/lib/facade/initial-admin-password."
    echo "Certificate fingerprint (to compare when logging in):"
    journalctl -u facaded --no-pager | grep -o "fingerprint [0-9a-f]*" | tail -1 || true
else
    token=${2:-}
    [[ -n $token ]] || die "usage: $0 agent <join-token>"
    mkdir -p /var/lib/facade-agent && chmod 700 /var/lib/facade-agent
    (umask 077 && printf '%s' "$token" > /var/lib/facade-agent/join-token)
    install -m 0644 "$here/facaded-agent.service" /etc/systemd/system/facaded-agent.service
    systemctl daemon-reload
    systemctl enable --now facaded-agent
    echo "facade agent running; check with: journalctl -u facaded-agent -f"
fi
