{
  description = "scurry - share one mouse and keyboard across machines over USB HID";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
    # Provides ESP-IDF and the riscv32 toolchain as derivations, so the C spike
    # needs no imperative clone into ~/esp and no install.sh writing to
    # ~/.espressif. Its nixpkgs is deliberately NOT followed: it pins versions
    # its prebuilt Espressif toolchains are known to work against.
    nixpkgs-esp-dev.url = "github:mirrexagon/nixpkgs-esp-dev";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, nixpkgs-esp-dev }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        # The dongle is an ESP32-C3: RISC-V, so it builds on upstream Rust with
        # a stock target. The nodes are ESP32-S3, which is Xtensa and needs the
        # esp-rs rustc fork via espup — deliberately NOT wired in here, so the
        # host and dongle toolchain stays reproducible. See doc/toolchain.md.
        # Nightly because the dongle's std target, riscv32imc-esp-espidf, is
        # tier 3: rustc knows the target spec but no std is distributed for it,
        # so it must be built from source with -Zbuild-std. That needs rust-src
        # and an unstable cargo. The host crates do not care either way.
        rustToolchain = pkgs.rust-bin.nightly.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
          targets = [ "riscv32imc-unknown-none-elf" ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain
            espflash   # flash and monitor the C3 over its USB Serial/JTAG
            esptool

            # esp-idf-sys's build script fetches and builds ESP-IDF itself into
            # .embuild/. nixpkgs has no esp-idf derivation, so these are its
            # host requirements. ldproxy is not packaged either and is installed
            # into .cargo/bin by `just dongle-setup`.
            git wget cmake ninja dfu-util python3 ncurses
          ];

          # scurry-ctl links CoreGraphics/ApplicationServices for event capture
          # on macOS. Those come from the darwin stdenv's apple-sdk now; the old
          # darwin.apple_sdk.frameworks.* stubs were removed from nixpkgs.

        };

        # Kept separate from the default shell: ESP-IDF plus its toolchain is a
        # large closure, and the host crates never need it.
        # ESP-IDF comes from nixpkgs-esp-dev's OWN package set, not via its
        # overlay applied to ours: the overlay wants python310, which unstable
        # has dropped. Taking the prebuilt package sidesteps that entirely.
        #
        # Kept out of the default shell because the IDF closure is large and the
        # host crates never need it.
        devShells.esp = pkgs.mkShell {
          packages = [
            nixpkgs-esp-dev.packages.${system}.esp-idf-riscv
            pkgs.espflash
            # The firmware build shells out to cargo to compile the Rust layout
            # engine for riscv32imc, so the toolchain has to be on PATH here too.
            rustToolchain
          ];
        };

        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "scurry-ctl";
          version = (pkgs.lib.importTOML ./Cargo.toml).workspace.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "-p" "scurry-ctl" ];
        };
      });
}
