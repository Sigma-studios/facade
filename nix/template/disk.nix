# One disk, wiped by nixos-anywhere: GPT with a BIOS boot partition and an EFI
# system partition, so GRUB boots it whether the provider starts machines with
# BIOS or UEFI, and the rest as ext4.
{ lib, ... }:
{
  disko.devices.disk.main = {
    # FIXME: the machine's disk (`lsblk` in the rescue system): /dev/sda, /dev/vda, /dev/nvme0n1…
    device = lib.mkDefault "/dev/sda";
    type = "disk";
    content = {
      type = "gpt";
      partitions = {
        bios = {
          size = "1M";
          type = "EF02";
        };
        esp = {
          size = "512M";
          type = "EF00";
          content = {
            type = "filesystem";
            format = "vfat";
            mountpoint = "/boot";
            mountOptions = [ "umask=0077" ];
          };
        };
        root = {
          size = "100%";
          content = {
            type = "filesystem";
            format = "ext4";
            mountpoint = "/";
          };
        };
      };
    };
  };
}
