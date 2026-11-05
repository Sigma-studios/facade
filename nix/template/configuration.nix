{
  lib,
  modulesPath,
  ...
}:
{
  imports = [
    (modulesPath + "/installer/scan/not-detected.nix")
    # Virtio drivers: most VPS run on KVM.
    (modulesPath + "/profiles/qemu-guest.nix")
  ]
  # Written by nixos-anywhere with
  #   --generate-hardware-config nixos-generate-config ./hardware-configuration.nix
  # (then `git add` it: a flake only sees files git knows).
  ++ lib.optional (builtins.pathExists ./hardware-configuration.nix) ./hardware-configuration.nix;

  services.facade = {
    server = {
      enable = true;
      settings = {
        public_host = "203.0.113.7"; # FIXME: the machine's public IP
        # The web UI at https://facade.example.com with a Let's Encrypt certificate,
        # once an A record points here. Without it: https://<public_host>:7701.
        api_domain = "facade.example.com"; # FIXME, or remove
        acme.email = "you@example.com"; # FIXME: where Let's Encrypt sends expiry notices
      };
    };
    openFirewall = true;
    # facade tells how far this system's nixpkgs is behind the branch (keep it
    # the same as the nixpkgs input in flake.nix).
    nixpkgsBranch = "nixos-26.05";
  };

  # GRUB on the BIOS boot partition and, as the removable fallback, on the EFI
  # system partition (disk.nix): boots either way.
  boot.loader.grub = {
    enable = true;
    efiSupport = true;
    efiInstallAsRemovable = true;
  };

  networking.hostName = "facade";
  # Most providers hand out the IPv4 address by DHCP. Static IPv6 (or IPv4):
  # networking.interfaces.<name>.ipv6.addresses = [ { address = "…"; prefixLength = 64; } ];
  networking.useDHCP = lib.mkDefault true;

  services.openssh = {
    enable = true;
    settings.PasswordAuthentication = false;
  };
  users.users.root.openssh.authorizedKeys.keys = [
    "ssh-ed25519 AAAA... you@laptop" # FIXME: your public key
  ];

  # Keeps the system lean: old generations and unreferenced store paths go.
  nix = {
    settings.experimental-features = [
      "nix-command"
      "flakes"
    ];
    gc = {
      automatic = true;
      dates = "weekly";
      options = "--delete-older-than 30d";
    };
  };

  system.stateVersion = "26.05";
}
