{
  description = "Offline UEFI POS appliance - pinned dev environment";

  inputs.nixpkgs.url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
    in {
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          rustup          # honours rust-toolchain.toml
          qemu            # qemu-system-x86_64
          OVMF.fd         # UEFI firmware for QEMU
          OVMFFull.fd     # Secure Boot + SMM build (sb-test)
          python3Packages.virt-firmware  # virt-fw-vars: enroll SB keys
          sbsigntool      # sbsign / sbverify
          efitools        # cert-to-efi-sig-list, sign-efi-sig-list
          openssl
          python3
          socat
          limine          # Tier B bootloader (BOOTX64.EFI)
          dosfstools      # fsck.fat for the FAT driver tests
        ];
        OVMF_CODE = "${pkgs.OVMF.fd}/FV/OVMF_CODE.fd";
        OVMF_VARS = "${pkgs.OVMF.fd}/FV/OVMF_VARS.fd";
        OVMF_SB_CODE = "${pkgs.OVMFFull.fd}/FV/OVMF_CODE.fd";
        OVMF_SB_VARS = "${pkgs.OVMFFull.fd}/FV/OVMF_VARS.fd";
        LIMINE_DIR = "${pkgs.limine}/share/limine";
        shellHook = ''
          export SOURCE_DATE_EPOCH=''${SOURCE_DATE_EPOCH:-1700000000}
        '';
      };
    };
}
