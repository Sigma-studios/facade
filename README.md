# facade

Self-hosted app platform for your own VPSes, in Rust. One server, any number of
nodes; apps from Docker images, git repositories (Dockerfile, cargo binary, or static
files); domains with automatic HTTPS; encrypted env vars; logs, metrics and Discord
alerts; everything scriptable from a CLI with `--json` output.

```
                         ┌───────────── facaded server ─────────────┐
 browsers ── :80/:443 ──▶│ ingress (routes by host, ACME certs)     │
 facade CLI ── :7701 ───▶│ API          SQLite + encrypted secrets  │
                         │ hub ◀── agents dial in on :7700 (TLS) ───┼──┐
                         │ in-process agent → Docker on this host   │  │
                         └──────────────────────────────────────────┘  │
                         ┌───────────── facaded agent ──────────────┐  │
                         │ Docker on another VPS                    │──┘
                         └──────────────────────────────────────────┘
```

* **Agents always dial the server** over TLS pinned to the server's certificate, so
  nodes need no open ports (they can sit behind NAT). Traffic for an app on another
  node is tunnelled back through a connection its agent opens on request.
* The server sends each agent its **complete desired state** on connection; the agent
  converges Docker to it (and re-checks every two minutes), so restarts and network
  blips heal by themselves.
* Containers get their own bridge network, `no-new-privileges`, optional CPU/memory
  limits, log rotation, and are only reachable from the ingress unless you publish
  ports.

## Crates

| crate | binary | what |
|---|---|---|
| `facade-core` | | spec types, wire protocol, manifest format |
| `facade-daemon` | `facaded` | `facaded server` (control plane + local agent), `facaded agent` |
| `facade-cli` | `facade` | the CLI |

## Install on a VPS (Ubuntu)

```sh
deploy/build-linux.sh            # static binaries in dist/amd64 (needs Docker locally)
scp -r dist/amd64 root@VPS:/root/facade
ssh root@VPS /root/facade/install.sh server VPS_PUBLIC_IP
```

The installer sets up Docker if needed, writes `/etc/facade/server.toml` (see
`deploy/server.example.toml` for every option) and starts
`facaded`. Then from your machine (`cargo install --path crates/facade-cli`):

```sh
facade login https://VPS_PUBLIC_IP:7701     # user admin, password in /var/lib/facade/initial-admin-password
facade passwd
```

Add `email` under `[acme]` for Let's Encrypt. Certificates are requested only once a
domain resolves, so you can assign domains before DNS exists; with a wildcard record
(`*.example.com → VPS`) any subdomain works the moment you assign it.

### More servers

```sh
facade node add eu-2                 # prints a join token
# on the new VPS, with the same dist/ folder:
/root/facade/install.sh agent <token>
```

Point domains of apps on `eu-2` at the **main** server (that is where the ingress
runs); published raw ports (`--publish`) are on the node itself.

## Using it

```sh
# a registry image
facade app create whoami --image traefik/whoami --port 80 --domain whoami.example.com
facade deploy whoami

# a static landing page from a repository directory
facade app create landing --git https://github.com/me/site --static public --domain example.com
facade deploy landing

# a cargo binary (bevy_ensemble's signalling server + TURN relay)
facade app create signalling \
  --git https://github.com/Sigma-studios/bevy_ensemble \
  --rust-package bevy_ensemble_webrtc --rust-bin bevy_ensemble_webrtc_server \
  --no-default-features --features server \
  --port 9090 --domain signal.example.com \
  --publish 3478:3478/udp --publish 49160-49260:49160-49260/udp \
  --memory 256
facade env set signalling SIGNALLING_ADDR=0.0.0.0:9090 TURN_PUBLIC_IP=VPS_PUBLIC_IP --public
facade env set signalling TURN_PASSWORD        # prompts; sensitive by default
facade deploy signalling                      # clients connect to wss://signal.example.com/ws
```

| | |
|---|---|
| `facade status` / `facade alerts` | nodes, apps, CPU/memory, firing alerts |
| `facade alerts config --webhook URL --mention @here` / `facade alerts test` | Discord notifications |
| `facade logs <app> -f` | container logs |
| `facade deployments <app> [--id ID]` | build history and logs |
| `facade metrics <app>` / `facade node metrics <node>` | last hour of samples |
| `facade env ls <app> [--reveal]` / `env set` / `env unset` | variables; sensitive unless `--public` |
| `facade domain add <app> <domain>` | route a host name to an app |
| `facade app update <app> --memory 512 --cpus 1 --node eu-2 …` | change settings (moving nodes works) |
| `facade stop` / `start` / `restart` / `app rm` | lifecycle |
| `facade export -o facade.toml` / `facade apply -f facade.toml` | config as code |

Apps run from their first `facade deploy`; until then they are only configuration.
`deploy` always rebuilds from the latest commit (or re-pulls the image); config and env
changes restart the container without rebuilding.

### Alerts on Discord

Create a webhook in a channel (Edit channel → Integrations → Webhooks → New Webhook),
then:

```sh
facade alerts config --webhook https://discord.com/api/webhooks/… --mention @here
facade alerts test
```

A message is posted when a problem lasts a minute (node offline, app crashed or failed
to deploy, disk ≥ 90 %, memory ≥ 95 %, app at its memory/CPU limit) and again when it
clears. Problems tag the `--mention`s (`@here`, `@everyone`, `user:<id>`, `role:<id>`,
repeatable; copy ids with Discord's developer mode); resolutions tag nobody.
`--no-mentions` turns tagging off. The webhook is stored encrypted on the server.

### For scripts and AI agents

Every command accepts `--json`. Instead of `facade login`, set `FACADE_SERVER`,
`FACADE_TOKEN` (and `FACADE_FINGERPRINT` for a self-signed server).

### Export / restore

`facade export` writes every node and app as TOML. Variables marked sensitive appear
as `{ sensitive = true }` without their value. `facade apply` on a fresh server
recreates everything, prints join tokens for the nodes it had to create and lists the
secrets still to set. Applying is idempotent and never deletes anything.

## Not there yet

* Web UI (the API is ready for it).
* Users other than admins, and per-app permissions.
* Persistent volumes for apps (databases).
* Private git repositories (deploy keys).
* Uploading a prebuilt executable as a source.
