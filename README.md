# facade

Self-hosted app platform for your own VPSes, in Rust. One server, any number of
nodes; apps from Docker images or git repositories, private ones included (Dockerfile,
cargo binary or static files); stacks of services deployed together from a monorepo
(its docker-compose file, or services defined in facade); databases with volumes;
domains with automatic HTTPS; encrypted env vars; builds pinned to commits, with a
build queue, health checks and rollbacks; logs, metrics and Discord alerts;
everything scriptable from a CLI with `--json` output.

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
  unless you publish ports or link another app to them.

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

* **A git repository**, public or [private](#private-repositories). facade clones it
  and scans it for what can be deployed (`facade scan <url>` shows the list): its
  Dockerfiles (also deeper in a monorepo, like `apps/api/Dockerfile`), Rust binaries
  (with the features they need, the port they listen on and the environment variables
  they read), Python programs (a script with a `__main__` block, a package with
  `__main__.py` or a `pyproject.toml` command; dependencies from `requirements.txt`
  and/or `pyproject.toml`; the version from `.python-version` or `requires-python`)
  or a static site. The best match is used unless you pick a build yourself
  (`--dockerfile`, `--rust-bin`, `--python`, `--static`). The scan also suggests a
  command when needed: a Python server whose `--host` defaults to `127.0.0.1` gets
  `--host 0.0.0.0`, since loopback is unreachable from outside its container. A
  repository holding several services (a monorepo with a docker-compose file, say) is
  better deployed as a [stack](#stacks-monorepos).
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
facade registry login ghcr.io --user me           # token with read:packages
facade app create blog --image ghcr.io/me/blog --port 3000 --domain example.com \
  --redirect www.example.com --volume data:/data
```

`--command` replaces the image's command (`CMD`), for any build: with a scanned
Python server it is what the scan suggests (`python server.py --host 0.0.0.0`);
`--arg` (repeatable) gives an uploaded executable its arguments.

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
| `facade app update <app> --command "./worker" --release "./migrate up"` | command, and what runs before each deploy |
| `facade app update <app> -v data:/data --link db` | volumes, and apps it reaches by name |
| `facade app update <app> --redirect www.example.com` | domains redirecting to the main one |
| `facade deploy <app> [--commit REF] [--rebuild]` / `rollback` / `cancel` | deploys |
| `facade updates <app>` | commits on the app's branch since its deployed one |
| `facade builds` | what each node builds, what waits, recent deployments |
| `facade db create <name> --kind postgres --link <app>` | a database for an app |
| `facade stack create shop --git <url>` / `stack show` / `stack plan` | a stack of services |
| `facade stack service update shop api --domain … --port 8080` | a stack service's settings |
| `facade git key` / `git token` / `git ls` | access to private repositories |
| `facade registry login ghcr.io --user …` / `registry ls` / `registry rm` | private container registries |
| `facade volume ls` / `export` / `import` / `rm` / `node prune` | volumes and disk space |
| `facade backup ls` / `run` / `get` / `restore` / `server` | backups |
| `facade node ports <node>` | what listens on a node, and who holds each port |
| `facade user add/ls/set/rm` / `app owner` / `app grant` / `whoami` | accounts and who manages what |
| `facade server` / `server set` | the web UI's domain, ACME contact, direct API port |
| `facade stop` / `start` / `restart` / `app rm [--volumes]` | lifecycle |
| `facade export -o facade.toml` / `facade apply -f facade.toml` | config as code |

Apps run from their first `facade deploy`; until then they are only configuration.
Config and env changes restart the container without rebuilding.

### Deploys, commits and rollbacks

`facade deploy` resolves the branch's latest commit on the server, records it, and has
the node build exactly that commit (submodules included). Images are tagged by commit,
so a node that rebuilds later (after a cleanup, or when the app moves) builds the same
code, and deploying a commit that was built before reuses its image:

```sh
facade deploy api                      # latest commit of the app's branch
facade deploy api --commit v1.4.2      # a tag, a branch, or a full commit hash
facade rollback api                    # the previous successfully deployed commit
facade deploy api --rebuild            # build again, without Docker's layer cache
```

Each node builds one app at a time; other deployments wait in its queue (`facade
builds`, or the Builds page). `facade cancel <app>` stops a queued or building
deployment: the running version stays, and the app does not move to the cancelled
commit later. Builds are stopped after an hour.

A **release command** (`--release "./migrate up"`) runs with `sh -c` in a one-off
container of the new build, with the app's variables, volumes and links, before the
new version replaces the running one; if it fails, the deploy stops and the running
version stays. It is the place for database migrations.

A **health check** (`--health "curl -fs localhost:8080/health"`, or the image's own
`HEALTHCHECK`) tells when the app is ready: a deploy waits for it to pass, and fails if
it does not. Without one, a deploy waits for the container to stay up a few seconds.

### Private repositories

facade needs read access to private repositories, for scans and builds alike:

```sh
# a deploy key for one repository: facade generates it, you add the public half
# on GitHub (Settings → Deploy keys; read-only is enough)
facade git key shop --repo https://github.com/me/shop

# or one access token for every repository of an account or organisation
# (GitHub: a fine-grained token with read access to Contents)
facade git token sigma --scope github.com/Sigma-studios      # prompts for the token
```

The most specific credential covering a repository is used, for HTTPS or SSH URLs
alike (a deploy key fetches over SSH, a token over HTTPS). Credentials are encrypted
on the server, sent to the node building the app over the pinned connection, never
written into URLs, logs or exports, and handed to `docker build` as build secrets so
Dockerfiles can fetch private dependencies without baking them into the image:
`RUN --mount=type=secret,id=gitconfig,target=/root/.gitconfig …` (tokens) or
`RUN --mount=type=ssh …` (keys); facade's generated Rust builds do this for git
dependencies. Host keys of GitHub, GitLab and Bitbucket are built in; other hosts are
remembered on first use. In the web UI, a scan that is refused offers to create the
deploy key on the spot. URLs with a password or token in them are refused.

### Databases, volumes and links

A **volume** (`-v data:/var/lib/data`) keeps what an app writes there across restarts
and deploys. Volumes live on the app's node; an app with volumes moves to another node with
`--move-data`, which copies them over (see [Volumes and backups](#volumes-and-backups)).
`facade app rm <app> --volumes` deletes them with the app; otherwise they stay, listed
as orphaned in `facade volume ls` until `facade volume rm`.

A **link** (`--link db`, or `--link shop-db:db` to use another name) lets an app reach
another app of the same node by name, over a network private to the two of them:
`postgres://…@db:5432`. Apps that are not linked cannot reach each other.

A variable can **refer to another app's variable**: `${{db.DATABASE_URL}}` is replaced
by the `DATABASE_URL` of app `db` when the app starts, and apps referring to a variable
restart when it changes, so a password lives in one place.

`facade db create` puts the three together: an official image (PostgreSQL, MySQL,
MariaDB or Redis), a volume for its data, a generated password, and a connection URL
for the apps it is linked to.

```sh
facade app create api --git https://github.com/me/shop --dockerfile api/Dockerfile \
  --port 8080 --domain api.example.com --release "./api migrate"
facade db create shop-db --kind postgres --link api   # api gets DATABASE_URL=${{shop-db.DATABASE_URL}}
facade deploy api
```

### Stacks (monorepos)

A **stack** deploys several services together: from one repository, at one commit, on
one node, where they reach each other by service name over a network of their own.
Its services come from the repository's docker-compose file, read again at every
deploy so the repository stays the source of truth, or are defined in facade when
there is no file.

```sh
facade stack create shop --git https://github.com/me/shop   # finds docker-compose.yml, shows the plan
facade stack service update shop web --domain shop.example.com --port 3000
facade deploy shop
facade logs shop.api -f                                    # services are apps named <stack>.<service>
```

A deploy resolves one commit and reads the file at that commit. Services whose build
context, settings and variables did not change are left running (their image is
keyed by their build context's content, not the commit). The others are **built
first, all of them**: if one fails, nothing is replaced. Then they are replaced in
`depends_on` order, each after the ones it depends on are healthy: Compose
`healthcheck:`s are used, and the usual database images (PostgreSQL, MySQL, MariaDB,
Redis, MongoDB) get a readiness check of their own. `facade stack plan shop` shows what
a deploy would do; rollbacks, cancels and the build queue work on the whole stack.

From the compose file: `image:` and `build:` (with its context and Dockerfile),
`command`, `environment` (interpolated like Compose does, from the stack's variables
and a committed `.env`), `ports` (the first one is where HTTP for the service's domains
goes; nothing is published on the node's public address unless asked), named volumes
(shared by the services naming them, as in Compose), `depends_on`, `healthcheck` and
resource limits. Passwords the file expects from the environment (`${DB_PASSWORD}`)
are generated once and kept as stack variables (`facade env ls shop`). Bind mounts,
`env_file`, build arguments and profiles are left out, and the plan says so. What
Compose has no words for goes in an `x-facade:` section of the service:

```yaml
services:
  api:
    build: ./api
    depends_on: [db]
    x-facade:
      domains: [api.example.com]
      http_port: 8080
      release: ./api migrate        # runs before each deploy replaces it
      memory: 512                   # MB
      publish: ["3478:3478/udp"]    # on the node's public address
```

The same settings can be set from facade instead (`facade stack service update`, or a
service's Settings tab), over the file's. Without a file, services are defined the same
way: `facade stack service add shop db --database postgres` (image, volume, password and
`DATABASE_URL`), `facade stack service add shop api --dockerfile api/Dockerfile --port
8080 --depends-on db`. A service's own variables are set on it (`facade env set
shop.api KEY=…`) and can refer to the others' (`${{shop.db.DATABASE_URL}}`).

Several standalone apps can also come from one repository, each with its own
Dockerfile and build context: the scan finds Dockerfiles deeper in a monorepo, and uses
the repository root as the context when the Dockerfile copies files from there.

### Disk space

Builds leave cache behind and every deploy leaves an image. Each node cleans up on its
own: every 6 hours it removes dangling images and build cache unused for a week, and
when its disk is 85 % full it also drops build cache unused for a day and the images
kept for rollbacks (the two previous builds of each app), and at 90 % the whole build
cache. `facade node prune <node> [--aggressive]` (or the Nodes page) does it now; the
Nodes page shows images, build cache and volumes per node.

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
* `facade app rm` keeps an app's volumes unless `--volumes`; leftovers are listed by
  `facade volume ls` and deleted with `facade volume rm <node> <name>`.
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

### Private registries

```sh
facade registry login ghcr.io --user me     # prompts for a token (read:packages)
```

Logins are stored encrypted on the server and sent to nodes for image pulls and
builds (a Dockerfile's `FROM` can name a private image). Changing one restarts
nothing. Like git credentials, a login belongs to someone: an app's pulls and builds
use its owner's (the admins' for the admins' apps).

### Ports

Each node reports every port listening on it and the program holding it
(`facade node ports <node>`, or the Nodes page). facade refuses to publish or bind a
port another app of the node declares, and refuses to start an app whose ports are
held by something else, naming it (`port 9090/tcp is in use on node main by
bevy_ensemble_w (pid 2115957)`).

A deploy whose ports another program holds builds first, then waits (up to 30
minutes) and says so in its log: `built; waiting for 9090/tcp held by
bevy_ensemble_w (pid 2115957)`. That is how a service moves into facade: deploy it
while the old one still serves, stop the old one once the build is done, and the
new one starts within a second. With `--host-network true`, `--publish` declares
the ports the app binds itself (no remapping), so they are checked too.

If a node's HTTP(S) entry point cannot bind 80 or 443 (another web server holds
them), it says so in `facade status` and as an alert, and retries every 5 seconds:
stopping the other server is enough for facade to take over.

**TLS ports.** For protocols that are not HTTP but run over TLS (IMAPS, SMTPS…),
`--tls-port public:app` (repeatable, or *TLS ports* in the settings form) has the
entry point listen on the node's public TCP port `public`, take TLS off with the
certificate of the app's domains (the one the client names if it is among them,
else the main domain's), and pass the connection on in the clear to the app's
port `app`. The app needs a domain, so it needs no certificate code of its own,
and renewals apply to the next connection.

```sh
# a mail server listening on 1143 (IMAP) and 1587 (submission) in its container
facade app update valet --tls-port 993:1143 --tls-port 465:1587
facade app update valet --no-tls-ports
```

Connections have no idle timeout (IMAP IDLE stays quiet for half an hour) and get
TCP keepalives. The app sees them come from 127.0.0.1. A TLS port conflicts with
other apps' ports like a published one, and one that cannot be bound is reported
like 80 and 443.

When ufw is active, the entry point opens a TLS port in it (`ufw allow
<port>/tcp`, commented *facade tls port*) as it starts serving it, and closes it
again once no app asks for it. Published ports need nothing: Docker lets them in
past ufw by itself.

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

`facade export` writes every node, app and stack as TOML. Variables marked sensitive appear
as `{ sensitive = true }` without their value. `facade apply` on a fresh server
recreates everything, prints join tokens for the nodes it had to create and lists the
secrets still to set. Applying is idempotent and never deletes anything.

### Upgrading

Servers and agents speak a versioned protocol; when it changes (it did with commit
pinning and the build queue), upgrade `facaded` on the server and on every node.

## Not there yet

* Users other than admins, and per-app permissions.
* Links across nodes: a database and the apps using it share a node.
* Backups to somewhere other than the server's disk (S3 and the like).
* Zero-downtime deploys (the new container starting next to the old one) and deploys
  on push (webhooks).
* Stacks and databases owned by members (admins create them for now).
