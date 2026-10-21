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

* **A git repository**, public or [private](#private-repositories). facade clones it
  and scans it for what can be deployed (`facade scan <url>` shows the list): its
  Dockerfiles (also deeper in a monorepo, like `apps/api/Dockerfile`), Rust binaries
  (with the features they need, the port they listen on and the environment variables
  they read) or a static site. The best match is used unless you pick a build
  yourself (`--dockerfile`, `--rust-bin`, `--static`). A repository holding several
  services (a monorepo with a docker-compose file, say) is better deployed as a
  [stack](#stacks-monorepos).
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
| `facade app update <app> --command "./worker" --release "./migrate up"` | command, and what runs before each deploy |
| `facade app update <app> -v data:/data --link db` | volumes, and apps it reaches by name |
| `facade deploy <app> [--commit REF] [--rebuild]` / `rollback` / `cancel` | deploys |
| `facade builds` | what each node builds, what waits, recent deployments |
| `facade db create <name> --kind postgres --link <app>` | a database for an app |
| `facade stack create shop --git <url>` / `stack show` / `stack plan` | a stack of services |
| `facade stack service update shop api --domain … --port 8080` | a stack service's settings |
| `facade git key` / `git token` / `git ls` | access to private repositories |
| `facade volume ls` / `volume rm` / `node prune` | volumes and disk space |
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
and deploys. Volumes live on the app's node, so an app with volumes does not move to
another node (remove the volume from the app first; its data stays on the old node).
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

`facade export` writes every node, app and stack as TOML. Variables marked sensitive appear
as `{ sensitive = true }` without their value. `facade apply` on a fresh server
recreates everything, prints join tokens for the nodes it had to create and lists the
secrets still to set. Applying is idempotent and never deletes anything.

### Upgrading

Servers and agents speak a versioned protocol; when it changes (it did with commit
pinning and the build queue), upgrade `facaded` on the server and on every node.

## Not there yet

* Users other than admins, and per-app permissions.
* Links and volumes across nodes: a database and the apps using it share a node, and
  volume data does not move with an app.
* Scheduled database backups.
* Zero-downtime deploys (the new container starting next to the old one) and deploys
  on push (webhooks).
