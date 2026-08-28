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
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
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
