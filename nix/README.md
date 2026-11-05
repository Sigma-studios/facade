# facade on NixOS

The flake at the root of this repository builds `facaded` and `facade` from
source (web UI included) and has a NixOS module, `services.facade`, setting up
what `deploy/install.sh` does on Ubuntu: Docker, `facaded` (or `facaded-agent`),
`facaded-ingress` serving the machine's apps on its own, and
`facade-volumes.service` mounting volumes with a size at boot, before Docker.

```sh
nix build github:Sigma-studios/facade#facaded     # or #facade, the CLI
```

## A new VPS, straight into NixOS

No NixOS needed beforehand: [nixos-anywhere](https://github.com/nix-community/nixos-anywhere)
installs it over SSH on any Linux machine reachable as root (the provider's
Ubuntu image, or its rescue system), wiping its disk. On your machine, with Nix:

```sh
mkdir facade-host && cd facade-host
nix flake init -t github:Sigma-studios/facade
git init && git add .
```

Fill in the `FIXME`s: in `configuration.nix` your SSH public key, the machine's
public IP, the domain of the web UI and the Let's Encrypt e-mail; in `disk.nix`
the disk (`lsblk` on the machine: `/dev/sda`, `/dev/vda`, `/dev/nvme0n1`…); in
`flake.nix` `aarch64-linux` for an ARM machine. The disk gets GPT with both a
BIOS boot partition and an EFI one, so it boots on providers starting machines
either way, and ext4 for the rest. Then:

```sh
nix run github:nix-community/nixos-anywhere -- --flake .#facade \
    --generate-hardware-config nixos-generate-config ./hardware-configuration.nix \
    --target-host root@IP
git add hardware-configuration.nix
```

facade is built from source: on your machine if it can build for the target
(Linux, same CPU, or a remote builder), else on the target itself
(`--build-on remote`; give it 4 GB of memory or more). Once the machine has
rebooted, log in as `admin` with the password in
`/var/lib/facade/initial-admin-password`, as on Ubuntu (see the main README):

```sh
ssh root@IP cat /var/lib/facade/initial-admin-password
facade login https://IP:7701        # or https://facade.example.com once its A record points here
facade passwd
```

Keep the `facade-host` folder (in a git repository of yours): it is the
machine's whole configuration.

## Into an existing NixOS flake

```nix
{
  inputs.facade = {
    url = "github:Sigma-studios/facade";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, facade, ... }: {
    nixosConfigurations.my-server = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        facade.nixosModules.default
        {
          services.facade = {
            server = {
              enable = true;
              settings = {
                public_host = "203.0.113.7";
                api_domain = "facade.example.com";
                acme.email = "you@example.com";
              };
            };
            openFirewall = true;            # 80, 443, and 7700/7701 on the server
            nixpkgsBranch = "nixos-26.05";  # what facade compares the system's nixpkgs with
          };
        }
      ];
    };
  };
}
```

`settings` is `/etc/facade/server.toml` (every option: `deploy/server.example.toml`;
`ingress = "external"` is the default here). The file is Nix's, read-only: the
server never writes it. `facade server set --api-domain … --acme-email …` (or
Settings → Server access) still works: it keeps those two in the server's
database, which wins over `settings` from then on.

An extra node, with the token from `facade node add <name>`:

```nix
services.facade = {
  agent = {
    enable = true;
    joinTokenFile = "/run/secrets/facade-join";  # sops-nix, agenix, or a file you copy there
  };
  openFirewall = true;  # 80 and 443
};
```

The token is read only until the node has joined: the agent then keeps it in
`/var/lib/facade-agent/join-token`, and the secret can go. To join with another
token, delete that file and restart `facaded-agent`.

Other options: `services.facade.package` (default: this flake's `facaded`) and
`services.facade.cliPackage` (the `facade` CLI, installed system-wide; `null` for
none).

## Updating

Nix owns the binaries. facade sees it runs from `/nix/store` and never replaces
itself: `facade server update` and Settings → Updates only say what is published,
and nodes on NixOS are not sent builds. Update the flake input instead, and
rebuild:

```sh
nix flake update facade
nixos-rebuild switch --flake .#facade --target-host root@IP    # or on the machine itself
```

The rebuild restarts `facaded` (apps keep running: they belong to Docker), and
`facaded-ingress` when facade itself changed, which closes open connections.

Every node needs updating this way, the server first: the server accepts agents
from the previous protocol version on.

## Testing

`nix flake check` builds both binaries and boots a VM with a facade server
(needs KVM; also `nix build .#checks.x86_64-linux.nixos -L`): the CLI logs in, an
app keeps its data in a volume with a size, and all of it comes back after a
reboot. CI runs it on every push (`.github/workflows/build.yml`, job `nix`).
