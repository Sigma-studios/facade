#!/usr/bin/env bash
# Installs facade on an Ubuntu/Debian machine, as root.
#
#   install.sh server [public_host] [--domain facade.example.com] [--email you@example.com]
#       the control plane (also runs apps on this machine). With --domain, the web UI
#       is served at that domain with a Let's Encrypt certificate as soon as its DNS
#       points here; --email is where Let's Encrypt sends expiry notices.
#   install.sh agent <join-token>
#       an extra node, token from `facade node add <name>`
#
# Expects the Linux `facaded` binary next to this script (see deploy/build-linux.sh),
# or its path in $FACADED.
set -euo pipefail

role=${1:-}
shift || true
here=$(cd "$(dirname "$0")" && pwd)
binary=${FACADED:-$here/facaded}

die() { echo "error: $*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || die "run as root"
[[ -x $binary ]] || die "no facaded binary at $binary (set FACADED=/path/to/facaded)"
[[ $role == server || $role == agent ]] ||
    die "usage: $0 server [public_host] [--domain D] [--email E] | agent <join-token>"

positional=()
domain=""
email=""
while [[ $# -gt 0 ]]; do
    case $1 in
        --domain) domain=${2:?--domain needs a value}; shift 2 ;;
        --email) email=${2:?--email needs a value}; shift 2 ;;
        -*) die "unknown option $1" ;;
        *) positional+=("$1"); shift ;;
    esac
done

if ! command -v docker >/dev/null; then
    echo "installing Docker…"
    curl -fsSL https://get.docker.com | sh
fi
if ! command -v git >/dev/null; then
    apt-get update -qq && apt-get install -y -qq git
fi

install -m 0755 "$binary" /usr/local/bin/facaded
if [[ -x $here/facade ]]; then
    install -m 0755 "$here/facade" /usr/local/bin/facade
fi

open_ports() {
    if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
        for port in "$@"; do ufw allow "$port"; done
    fi
}

if [[ $role == server ]]; then
    public_host=${positional[0]:-$(hostname -I | awk '{print $1}')}
    mkdir -p /etc/facade
    if [[ ! -f /etc/facade/server.toml ]]; then
        {
            echo "public_host = \"$public_host\""
            if [[ -n $domain ]]; then
                echo
                echo "# Web UI and API with a real certificate (later: facade server set --api-domain)."
                echo "api_domain = \"$domain\""
            fi
            echo
            echo "[acme]"
            if [[ -n $email ]]; then
                echo "email = \"$email\""
            else
                echo '# email = "you@example.com"'
            fi
        } > /etc/facade/server.toml
    elif [[ -n $domain || -n $email ]]; then
        echo "note: /etc/facade/server.toml exists and was kept; once logged in, run"
        echo "  facade server set${domain:+ --api-domain $domain}${email:+ --acme-email $email}"
    fi
    install -m 0644 "$here/facaded.service" /etc/systemd/system/facaded.service
    systemctl daemon-reload
    systemctl enable --now facaded
    open_ports 80/tcp 443/tcp 7700/tcp 7701/tcp
    sleep 2
    echo
    echo "facade server running. Log in as admin with the password in"
    echo "/var/lib/facade/initial-admin-password (deleted once you change it):"
    if [[ -n $domain ]]; then
        echo "  https://$domain, once an A record for $domain points to $public_host"
        echo "    (its certificate follows within a minute; check with: facade domains)"
        echo "  until then https://$public_host:7701 (self-signed certificate)"
    else
        echo "  https://$public_host:7701 (self-signed certificate), in a browser or with"
        echo "  facade login https://$public_host:7701"
        echo "  For a real certificate later: facade server set --api-domain facade.example.com"
    fi
    echo "The certificate fingerprint of port 7701, to compare when asked:"
    journalctl -u facaded --no-pager | grep -o "fingerprint [0-9a-f]*" | tail -1 || true
else
    token=${positional[0]:-}
    [[ -n $token ]] || die "usage: $0 agent <join-token>"
    mkdir -p /var/lib/facade-agent && chmod 700 /var/lib/facade-agent
    (umask 077 && printf '%s' "$token" > /var/lib/facade-agent/join-token)
    install -m 0644 "$here/facaded-agent.service" /etc/systemd/system/facaded-agent.service
    systemctl daemon-reload
    systemctl enable --now facaded-agent
    open_ports 80/tcp 443/tcp
    echo "facade agent running; check with: journalctl -u facaded-agent -f"
fi
