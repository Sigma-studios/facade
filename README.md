# facade

Self-hosted app platform for your own VPSes, in Rust. One server, any number of
nodes; apps from Docker images, git repositories (Dockerfile, cargo binary, or static
files); domains with automatic HTTPS; encrypted env vars; logs, metrics and Discord
alerts; everything scriptable from a CLI with `--json` output.

```
 facade CLI / web UI ── :7701 ──▶ facaded server: API, SQLite + encrypted secrets,
                                  certificates (ACME), DNS records, alerts
                                        ▲  agents dial in on :7700 (pinned TLS)
                     ┌──────────────────┴───────────────────┐
 app.example.com ──▶ agent on main (the server's machine)   agent on eu-2 ◀── api.example.com
   :80/:443          Docker + HTTPS entry point             Docker + HTTPS entry point
```

* **Each node serves its own apps.** A domain points at the node its app runs on;
  that node's agent terminates HTTPS and forwards to the container, so traffic never
  goes through another server. Nodes need ports 80 and 443 open.
* **The server is the control plane.** It gets certificates and pushes them to the
  nodes that need them, keeps DNS records up to date through the DNS provider, and
  sends each agent its complete desired state when it connects. Agents converge
  Docker to it (and re-check every two minutes), so restarts and network blips heal
  by themselves; their apps stay up and reachable while the server is away.
* **Agents dial the server**, over TLS pinned to the server's certificate.
* Containers get their own bridge network, `no-new-privileges`, optional CPU/memory
  limits, log rotation, and are only reachable through their node's entry point
  unless you publish ports.

## Crates

| crate | binary | what |
|---|---|---|
| `facade-core` | | spec types, wire protocol, manifest format |
| `facade-daemon` | `facaded` | `facaded server` (control plane + local agent), `facaded agent` |
| `facade-cli` | `facade` | the CLI |
| `facade-ui` | | web UI (Leptos, WebAssembly), embedded in `facaded` |

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

Add `email` under `[acme]` for Let's Encrypt, then set up domains (below).

### More servers

```sh
facade node add eu-2                 # prints a join token
# on the new VPS, with the same dist/ folder:
/root/facade/install.sh agent <token>
```

The node's public IP is detected from where its agent connects; set it with
`facade node ip eu-2 203.0.113.20` if the internet reaches it at another address.

## Web UI

`facaded` serves the web UI on the API port: open `https://VPS_IP:7701` (your browser
warns about the self-signed certificate once) or, with `api_domain` set, that domain
with a real certificate. Log in with the same account as the CLI. Everything the CLI
does is there: apps with live logs, deploys and build output, environment variables,
settings and limits, nodes and their join commands, alerts and Discord settings,
export and apply.

### Working on the web UI

```sh
# terminal 1: a local server (config with ports 17700/17701 and acme disabled)
cargo run --bin facaded -- server -c /tmp/facade-dev/server.toml
# terminal 2: the UI with hot reload, proxying /api to that server
cd crates/facade-ui && trunk serve       # http://127.0.0.1:8080
```

`trunk build` refreshes what a debug `facaded` serves without restarting it; release
builds embed `crates/facade-ui/dist` (built by `deploy/build-linux.sh`).

## Using it

An app comes from one of three sources:

* **A git repository.** facade clones it and scans it for what can be deployed
  (`facade scan <url>` shows the list): its own Dockerfile, Rust binaries (with the
  features they need, the port they listen on and the environment variables they
  read) or a static site. The best match is used unless you pick a build yourself
  (`--dockerfile`, `--rust-bin`, `--static`).
* **A Linux executable** you upload (`--executable`, or drag and drop in the web
  UI). It runs in a small Debian image; it must be built for the node's CPU
  (`x86_64` or `aarch64`), and a static build (`--target x86_64-unknown-linux-musl`)
  runs anywhere.
* **A Docker image** from a registry (`--image`).

```sh
# bevy_ensemble's signalling server + TURN relay: the scan finds the server binary,
# its `server` feature, port 9090 and the TURN_* variables
facade app create signalling --git https://github.com/Sigma-studios/bevy_ensemble \
  --domain signal.example.com \
  --publish 3478:3478/udp --publish 49160-49260:49160-49260/udp --memory 256
facade env set signalling SIGNALLING_ADDR=0.0.0.0:9090 TURN_PUBLIC_IP=VPS_PUBLIC_IP --public
facade env set signalling TURN_PASSWORD        # prompts; sensitive by default
facade deploy signalling                      # clients connect to wss://signal.example.com/ws

# a program you built yourself
facade app create api --executable target/x86_64-unknown-linux-musl/release/api \
  --arg --port --arg 8080 --port 8080 --domain api.example.com
facade deploy api
facade app update api --executable target/x86_64-unknown-linux-musl/release/api   # new version

# a static landing page, and a registry image
facade app create landing --git https://github.com/me/site --domain example.com
facade app create whoami --image traefik/whoami --port 80 --domain whoami.example.com
```

### Teaching the scan a new language

Detection lives in `crates/facade-daemon/src/lang/`, one file per kind of project.
A new language is a `Detector` there (registered in `lang::detectors()`), plus, if
facade has to write its Dockerfile, a `Build` variant in `facade-core` and its
template in `lang::generated_dockerfile`.

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

### Domains and HTTPS

Give an app a domain (`--domain`, `facade domain add`, or its Settings tab). The
domain must point at the node the app runs on; that node serves it over HTTPS with
a certificate facade obtains and renews. `facade domains` (or the Domains page) shows,
for each domain, where it must point, whether it does, and its certificate.

**With Porkbun**, facade does the DNS too:

```sh
facade dns porkbun --api-key pk1_… --secret-key sk1_…   # porkbun.com → Account → API Access
```

and turn on *API Access* in each domain's settings at Porkbun. From then on facade
creates and updates each domain's A record (pointing at its app's node, and following
the app when it moves), removes the records it created when a domain goes away, and
gets one wildcard certificate (`example.com` + `*.example.com`) per domain through
DNS-01. Records you made yourself are never touched.

**With any other provider**, create one A record per domain, pointing at the node's
public IP (the Domains page says which). Each domain then gets its own certificate
through HTTP-01, as soon as the record resolves. Another provider can be supported
by implementing `server/dns::Provider` (five calls: list zones, set/delete an A
record, add/delete a TXT record).

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

* Users other than admins, and per-app permissions.
* Persistent volumes for apps (databases).
* Private git repositories (deploy keys).
