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
  that node's entry point (`facaded-ingress`) terminates HTTPS and forwards to the
  container, so traffic never goes through another server. Nodes need ports 80 and
  443 open.
* **The entry point is its own process.** The node's agent tells it what to serve;
  it keeps serving while the agent or the server restarts, and it updates itself by
  handing its sockets to the new version, so updates never drop connections.
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
deploy/build-linux.sh            # static binaries in dist/amd64 (needs Docker, or podman, and trunk)
scp -r dist/amd64 root@VPS:/root/facade
ssh root@VPS /root/facade/install.sh server VPS_PUBLIC_IP \
    --domain facade.example.com --email you@example.com
```

The installer sets up Docker if needed, writes `/etc/facade/server.toml` (see
`deploy/server.example.toml` for every option) and starts `facaded`. Point an A
record for the `--domain` at the server: within a minute the web UI is at
`https://facade.example.com` with a Let's Encrypt certificate. Log in as `admin`
with the password in `/var/lib/facade/initial-admin-password` (root only, never
logged), and change it: a banner insists until you do, and the file is deleted then.

Until the domain works, or without one, everything is also on port 7701 with a
self-signed certificate (`https://VPS_PUBLIC_IP:7701`; the installer prints its
fingerprint). From your machine (`cargo install --path crates/facade-cli`):

```sh
facade login https://facade.example.com     # or https://VPS_PUBLIC_IP:7701
facade passwd
```

The domain and the Let's Encrypt contact can be changed any time (`facade server set
--api-domain … --acme-email …`, or Settings → Server access). Once the domain has its
certificate, `facade server set --direct-api-local-only` closes port 7701 to the
outside: facade is then public only at its domain, and the port stays usable through
an SSH tunnel (`ssh -L 7701:127.0.0.1:7701 VPS`) as a way back in.

### Updating

```sh
deploy/build-linux.sh
deploy/upgrade.sh root@VPS          # or ubuntu@VPS with passwordless sudo
```

`upgrade.sh` first asks the new binary what it would do to the running apps (a new
version can change how containers are fingerprinted: they are then recreated, or
rebuilt from their branch's latest commit), and waits for a yes. Then it backs up
the database and key, keeps the previous binary as `facaded.prev`, installs, and
waits for the server to be healthy, going back to the previous binary if it is not
within a minute. Last, `systemctl reload facaded-ingress` hands the entry point over
to the new binary: it takes the listening sockets, and the old process finishes its
open connections (WebSockets included, up to an hour) before exiting. If the new
binary does not start, the old one keeps serving.

Apps run on through all of it: they belong to Docker. Other nodes update themselves:
when an agent connects to a server running another build of facaded (for the same
CPU), it downloads the server's binary, hands its node's entry point over to it and
restarts. The server accepts agents from the previous protocol version on, so an
update never locks nodes out.

Installs from before the separate entry point serve apps from inside `facaded`: add
`ingress = "external"` to `/etc/facade/server.toml`, install
`facaded-ingress.service`, then `systemctl restart facaded && systemctl enable --now
facaded-ingress`.

### More servers

```sh
facade node add eu-2                 # prints a join token
# on the new VPS, with the same dist/ folder:
/root/facade/install.sh agent <token>
```

The node's public IP is detected from where its agent connects; set it with
`facade node ip eu-2 203.0.113.20` if the internet reaches it at another address.

## Web UI

`facaded` serves the web UI at its API domain (`facade server set --api-domain`) with
a real certificate, and on the API port `https://VPS_IP:7701` (your browser warns
about the self-signed certificate once). Log in with the same account as the CLI. Everything the CLI
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
  read), Python programs (a script with a `__main__` block, a package with
  `__main__.py` or a `pyproject.toml` command; dependencies from `requirements.txt`
  and/or `pyproject.toml`; the version from `.python-version` or `requires-python`)
  or a static site. The best match is used unless you pick a build yourself
  (`--dockerfile`, `--rust-bin`, `--python`, `--static`). The scan also suggests
  arguments: a Python server whose `--host` defaults to `127.0.0.1` gets
  `--host 0.0.0.0`, since loopback is unreachable from outside its container.
* **A Linux executable** you upload (`--executable`, or drag and drop in the web
  UI). It runs in a small Debian image; it must be built for the node's CPU
  (`x86_64` or `aarch64`), and a static build (`--target x86_64-unknown-linux-musl`)
  runs anywhere.
* **A Docker image** from a registry (`--image`).

```sh
# bevy_ensemble's signalling server + TURN relay: the scan finds the server binary,
# its `server` feature, port 9090 and the TURN_* variables. On the host network, so
# relayed media skips Docker's NAT; --publish declares the ports it binds, so a
# conflict is caught before it starts.
facade app create signalling --git https://github.com/Sigma-studios/bevy_ensemble \
  --domain signal.example.com --host-network true \
  --publish 3478:3478/udp --publish 49160-49260:49160-49260/udp --memory 256
facade env set signalling TURN_PUBLIC_IP=VPS_PUBLIC_IP --public
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

# a Python server (found by the scan: server.py, port 8000, args --host 0.0.0.0),
# reached at its main domain; www. redirects there
facade app create poker --git https://github.com/me/poker --domain poker.example.com \
  --redirect www.poker.example.com

# a site keeping a SQLite database across deploys, from a private registry
facade auth registry ghcr.io --user me            # token with read:packages
facade app create blog --image ghcr.io/me/blog --port 3000 --domain example.com \
  --redirect www.example.com --volume data:/data
```

`--arg` (repeatable) passes arguments to the program: after the executable, Rust
binary or Python entry, or in place of the `CMD` of a Dockerfile or image.

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
| `facade updates <app>` | commits on the app's branch since its deployed one |
| `facade metrics <app>` / `facade node metrics <node>` | last hour of samples |
| `facade env ls <app> [--reveal]` / `env set` / `env unset` | variables; sensitive unless `--public` |
| `facade domain add <app> <domain>` | route a host name to an app |
| `facade app update <app> --memory 512 --cpus 1 --node eu-2 …` | change settings (moving nodes works) |
| `facade stop` / `start` / `restart` / `app rm [--delete-data]` | lifecycle |
| `facade volume ls` / `export` / `import` | volumes, as `.tar.gz` |
| `facade backup ls` / `run` / `get` / `restore` / `server` | backups |
| `facade auth registry\|git <host> --user …` / `auth ls` / `auth rm` | private registries and repositories |
| `facade node ports <node>` | what listens on a node, and who holds each port |
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

### Volumes and backups

`--volume data:/data` (repeatable) gives an app a directory that outlives its
containers: deploys, restarts and configuration changes keep its contents. Volumes
live on the app's node, as Docker volumes named `facade-<app>_<volume>`.

* `facade volume export <app> <volume>` downloads one as a `.tar.gz`; the app is
  paused for the copy, so files (a SQLite database and its WAL) are taken at one
  instant. `facade volume import <app> <volume> <file>` replaces its contents with
  a `.tar` or `.tar.gz` (owners and permissions kept) while the app is stopped:
  that is how existing data comes into facade.
* `facade app rm` keeps an app's volumes unless `--delete-data`; leftovers are
  listed by `facade volume ls` and deleted with `facade volume rm <app> --node <node>`.
* Moving an app with volumes to another node needs `--move-data`: the app stops,
  its volumes are copied to the new node, then deleted from the old one. If the copy
  fails, the app starts again where it was.

The server backs up every volume once a day and keeps the last 7 (`[backups]` in
`server.toml`), in `/var/lib/facade/backups`, together with a copy of its own
database and master key. They are on the server's disk: take copies elsewhere with
`facade backup get <app> <volume> <id>` and `facade backup server` (the database and
the key to its secrets, in one file). `facade backup run [app]` backs up now and
`facade backup restore <app> <volume> <id>` puts a backup back. A failed backup is
an alert.

### Private registries and repositories

```sh
facade auth registry ghcr.io --user me     # prompts for a token (read:packages)
facade auth git github.com --user me       # a token that can read the repositories
```

Logins are stored encrypted on the server and sent to nodes for pulls, clones
(`https://` URLs) and builds (a Dockerfile's `FROM` can use a private registry).
Changing one restarts nothing.

### Ports

Each node reports every port listening on it and the program holding it
(`facade node ports <node>`, or the Nodes page). facade refuses to publish or bind a
port another app of the node declares, and refuses to deploy or start an app whose
ports are held by something else, naming it (`port 9090/tcp is in use on node main
by bevy_ensemble_w (pid 2115957)`). With `--host-network true`, `--publish` declares
the ports the app binds itself (no remapping), so they are checked too.

If a node's HTTP(S) entry point cannot bind 80 or 443 (another web server holds
them), it says so in `facade status` and as an alert, and retries every 5 seconds:
stopping the other server is enough for facade to take over.

### Alerts on Discord

Create a webhook in a channel (Edit channel → Integrations → Webhooks → New Webhook),
then:

```sh
facade alerts config --webhook https://discord.com/api/webhooks/… --mention @here
facade alerts test
```

A message is posted when a problem lasts a minute (node offline, app crashed or failed
to deploy, disk ≥ 90 %, memory ≥ 95 %, app at its memory/CPU limit, entry point unable
to bind its port, failed backup) and again when it clears. Problems tag the `--mention`s (`@here`, `@everyone`, `user:<id>`, `role:<id>`,
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
* Backups to somewhere other than the server's disk (S3 and the like).
* Redeploying on `git push` (webhooks).
* Zero-downtime deploys: a changed app is stopped, then started again.
