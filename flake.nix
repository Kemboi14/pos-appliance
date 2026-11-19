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
          sbsigntool      # sbsign / sbverify
          efitools        # cert-to-efi-sig-list, sign-efi-sig-list
          openssl
          python3
          socat
        ];
        OVMF_CODE = "${pkgs.OVMF.fd}/FV/OVMF_CODE.fd";
        OVMF_VARS = "${pkgs.OVMF.fd}/FV/OVMF_VARS.fd";
        shellHook = ''
          export SOURCE_DATE_EPOCH=''${SOURCE_DATE_EPOCH:-1700000000}
        '';
      };
    };
}
