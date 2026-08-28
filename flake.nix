{
  description = "scurry - share one mouse and keyboard across machines over USB HID";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
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

        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "scurry-ctl";
          version = (pkgs.lib.importTOML ./Cargo.toml).workspace.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "-p" "scurry-ctl" ];
        };
      });
}
