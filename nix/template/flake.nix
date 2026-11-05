{
  description = "A machine running facade";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    facade = {
      url = "github:Sigma-studios/facade";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    disko = {
      url = "github:nix-community/disko";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      facade,
      disko,
      ...
    }:
    {
      # Install: nix run github:nix-community/nixos-anywhere -- --flake .#facade --target-host root@IP
      # Update:  nix flake update facade && nixos-rebuild switch --flake .#facade --target-host root@IP
      nixosConfigurations.facade = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux"; # FIXME: "aarch64-linux" on an ARM machine
        modules = [
          disko.nixosModules.disko
          facade.nixosModules.default
          ./disk.nix
          ./configuration.nix
        ];
      };
    };
}
