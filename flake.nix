{
  description = "facade: a self-hosted PaaS (facaded, the facade CLI, a NixOS module)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
    }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems =
        f:
        lib.genAttrs systems (
          system:
          f (
            import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            }
          )
        );
      build =
        pkgs:
        import ./nix/package.nix {
          inherit pkgs crane;
          # Only a clean checkout has a commit (like deploy/build-linux.sh).
          commit = self.rev or null;
        };
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          facade = build pkgs;
        in
        {
          inherit (facade) facaded facade;
          facade-ui = facade.ui;
          default = facade.facaded;
        }
      );

      # services.facade: see nix/README.md.
      nixosModules.default = import ./nix/module.nix self;
      nixosModules.facade = self.nixosModules.default;

      checks = forAllSystems (pkgs: {
        inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) facaded facade;
        # A VM running a facade server (needs KVM): `nix build .#checks.x86_64-linux.nixos -L`.
        nixos = import ./nix/test.nix { inherit pkgs self; };
      });

      # A host flake booting a VPS straight into facade (nixos-anywhere + disko).
      templates.default = {
        path = ./nix/template;
        description = "A NixOS machine running facade, installable on a VPS with nixos-anywhere";
        welcomeText = ''
          Fill in the FIXMEs in configuration.nix (SSH key, public IP, domain,
          e-mail, disk), then from any machine with Nix:

            nix run github:nix-community/nixos-anywhere -- --flake .#facade --target-host root@IP

          See https://github.com/Sigma-studios/facade/blob/main/nix/README.md
        '';
      };

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
