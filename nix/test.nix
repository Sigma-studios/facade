# A VM running a facade server from the NixOS module: the API answers, the CLI
# logs in with the generated admin password, the server's own node is online and
# knows Nix manages facade, an app keeps its data in a volume with a size, and
# all of it comes back after a reboot, the volume mounted again by
# facade-volumes.service before Docker starts the app.
#
# The VM has no internet: the app's image is built by Nix and pushed to a
# registry on the VM itself.
#
#   nix build .#checks.x86_64-linux.nixos -L     (needs KVM)
{ pkgs, self }:
let
  image = pkgs.dockerTools.buildImage {
    name = "localhost:5000/sleeper";
    tag = "1";
    copyToRoot = pkgs.buildEnv {
      name = "sleeper-root";
      paths = [ pkgs.busybox ];
      pathsToLink = [ "/bin" ];
    };
    config.Cmd = [
      "/bin/sleep"
      "infinity"
    ];
  };
in
pkgs.testers.runNixOSTest {
  name = "facade";

  nodes.machine = {
    imports = [ self.nixosModules.default ];

    virtualisation = {
      memorySize = 2048;
      diskSize = 4096;
    };

    services.facade = {
      server = {
        enable = true;
        settings = {
          public_host = "192.168.1.1";
          # Offline: no certificates, outside checks or vulnerability data to fetch.
          acme.enabled = false;
          checks.enabled = false;
          vulns.enabled = false;
        };
      };
      openFirewall = true;
      nixpkgsBranch = "nixos-unstable";
    };

    services.dockerRegistry = {
      enable = true;
      listenAddress = "127.0.0.1";
      port = 5000;
    };

    environment.systemPackages = [ pkgs.curl ];
  };

  testScript = ''
    import json

    def server_up():
        machine.wait_for_unit("facaded-ingress.service")
        machine.wait_for_unit("facaded.service")
        machine.wait_for_open_port(7701)
        machine.wait_until_succeeds("curl -kfsS https://127.0.0.1:7701/api/health", timeout=60)

    def node_online():
        machine.wait_until_succeeds(
            "facade node list | grep -E '^main \\(server\\) +online'", timeout=120
        )

    def volume_mounted():
        machine.wait_until_succeeds(
            "findmnt -rn -o TARGET | grep -E '^/var/lib/facade/agent/volumes/[^/]+/mnt$'",
            timeout=60,
        )

    start_all()
    server_up()
    machine.succeed("systemctl is-active facade-volumes.service")
    machine.succeed("test -L /etc/facade/server.toml")
    machine.succeed("grep -x 'ingress = \"external\"' /etc/facade/server.toml")

    with subtest("the CLI logs in with the generated admin password"):
        machine.wait_for_file("/var/lib/facade/initial-admin-password")
        machine.succeed(
            "facade login https://127.0.0.1:7701 --yes --password-stdin"
            " < /var/lib/facade/initial-admin-password"
        )

    with subtest("the server's node is online, on NixOS, managed by Nix"):
        node_online()

        def main_platform():
            nodes = json.loads(machine.succeed("facade node list --json"))
            return next(n for n in nodes if n["name"] == "main").get("platform")

        retry(lambda _: main_platform() is not None, timeout_seconds=120)
        platform = main_platform()
        assert platform["os"]["id"] == "nixos", platform
        assert platform["install"] == {
            "kind": "managed",
            "by": "nix",
            "hint": platform["install"].get("hint"),
        }, platform

    with subtest("a server running from /nix/store leaves updates to Nix"):
        update = json.loads(machine.succeed("facade server update --check --json"))
        assert "nix" in (update.get("managed") or ""), update

    with subtest("an app keeps its data in a volume with a size"):
        machine.wait_for_unit("docker-registry.service")
        machine.wait_for_open_port(5000)
        machine.succeed("docker load < ${image}")
        machine.succeed("docker push localhost:5000/sleeper:1")
        machine.succeed(
            "facade app create sleeper --image localhost:5000/sleeper:1 -v data:/data:64M"
        )
        machine.succeed("facade deploy sleeper >&2")
        volume_mounted()
        machine.succeed("facade exec sleeper -- sh -c 'echo kept > /data/file'")
        # Its own filesystem: 64M, not the VM's disk.
        size = machine.succeed("facade exec sleeper -- df -m /data | tail -1").split()[1]
        assert int(size) < 100, size

    with subtest("everything comes back after a reboot"):
        machine.shutdown()
        machine.start()
        server_up()
        machine.succeed("systemctl is-active facade-volumes.service")
        volume_mounted()
        node_online()
        machine.wait_until_succeeds(
            "facade exec sleeper -- cat /data/file | grep -x kept", timeout=120
        )
  '';
}
