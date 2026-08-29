{
  description = "scurry - share one mouse and keyboard across machines";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
    # ESP-IDF and the riscv32 toolchain as derivations, so the firmware needs no
    # imperative clone into ~/esp and no install.sh writing to ~/.espressif. Its
    # nixpkgs is deliberately NOT followed: it pins versions its prebuilt
    # Espressif toolchains are known to work against.
    nixpkgs-esp-dev.url = "github:mirrexagon/nixpkgs-esp-dev";
    # Bundles the full closure into a squashfs and mounts /nix/store via user
    # namespaces at runtime, so the binary's ELF interpreter and RUNPATH resolve
    # on any modern Linux box without Nix installed.
    nix-appimage = {
      url = "github:ralismark/nix-appimage";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-utils.follows = "flake-utils";
    };
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, nixpkgs-esp-dev, nix-appimage }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        lib = pkgs.lib;

        # Single source of truth so the flake cannot drift from a version bump.
        cargoVersion = (lib.importTOML ./Cargo.toml).workspace.package.version;

        # Nightly because the firmware's std target, riscv32imc-esp-espidf, is
        # tier 3: rustc knows the target spec but ships no std for it, so it must
        # be built from source with -Zbuild-std. The host crates do not care.
        rustToolchain = pkgs.rust-bin.nightly.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
          targets = [ "riscv32imc-unknown-none-elf" ];
        };

        # winit and eframe need these at build and run time on Linux. Missing
        # any of them shows up as a link error, or worse as a binary that builds
        # and then cannot open a window on the user's machine.
        linuxGuiDeps = with pkgs; [
          libxkbcommon
          wayland
          libGL
          xorg.libX11
          xorg.libXcursor
          xorg.libXrandr
          xorg.libXi
          # The tray talks to the desktop's StatusNotifierItem over D-Bus.
          dbus
          # libappindicator is what tray-icon falls back to on desktops without
          # a StatusNotifierItem host.
          libayatana-appindicator
          glib
          gtk3
        ];

        nativeBuildInputs = with pkgs; [ pkg-config ]
          ++ lib.optionals stdenv.hostPlatform.isLinux [ autoPatchelfHook ];
        buildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux linuxGuiDeps;

        mkScurry = { pname, cargoBuildFlags }: pkgs.rustPlatform.buildRustPackage {
          inherit pname cargoBuildFlags nativeBuildInputs buildInputs;
          version = cargoVersion;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # The workspace's tests are run by `nix flake check`; building the
          # release artifacts twice over is wasted CI minutes.
          doCheck = false;

          meta = with lib; {
            description = "Share one mouse and keyboard across machines";
            homepage = "https://github.com/ananthb/scurry";
            license = licenses.mit;
            platforms = platforms.unix;
          };
        };

        scurry-ctl = mkScurry {
          pname = "scurry-ctl";
          cargoBuildFlags = [ "-p" "scurry-ctl" ];
        };

        # icns is assembled from the generated PNGs with libicns rather than
        # macOS's iconutil, so the bundle can be built on a Linux runner too.
        iconIcns = pkgs.runCommand "scurry.icns" { nativeBuildInputs = [ pkgs.libicns ]; } ''
          png2icns $out \
            ${./assets/icon-16.png} ${./assets/icon-32.png} ${./assets/icon-48.png} \
            ${./assets/icon-128.png} ${./assets/icon-256.png} ${./assets/icon-512.png}
        '';

        scurry-tray = (mkScurry {
          pname = "scurry-tray";
          cargoBuildFlags = [ "-p" "scurry-tray" ];
        }).overrideAttrs (old: {
          postInstall = (old.postInstall or "") + lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
            app="$out/Applications/scurry.app"
            mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
            substitute ${./packaging/Info.plist} "$app/Contents/Info.plist" \
              --replace-fail '@VERSION@' '${cargoVersion}'
            cp ${iconIcns} "$app/Contents/Resources/icon.icns"
            cp $out/bin/scurry-tray "$app/Contents/MacOS/"
          '';
        });
      in
      {
        packages = {
          default = scurry-ctl;
          inherit scurry-ctl scurry-tray;
        }
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          release =
            let arch = if system == "x86_64-linux" then "amd64" else "arm64";
            in pkgs.runCommand "scurry-${arch}.tar.gz"
              { nativeBuildInputs = [ pkgs.gzip pkgs.patchelf ]; } ''
              mkdir -p scurry
              cp ${scurry-ctl}/bin/scurry-ctl scurry/
              cp ${scurry-tray}/bin/scurry-tray scurry/
              chmod +w scurry/scurry-ctl scurry/scurry-tray

              # Nix bakes an RPATH into /nix/store. On a machine without Nix the
              # loader follows it, finds nothing, and the binary dies before
              # main. Stripping it makes the binary use the system loader path
              # like any other distro binary.
              patchelf --remove-rpath scurry/scurry-ctl
              patchelf --remove-rpath scurry/scurry-tray

              cp ${./packaging/scurry-tray.desktop} scurry/scurry-tray.desktop
              cp ${./packaging/scurry.service} scurry/scurry.service
              cp ${./packaging/99-scurry-dongle.rules} scurry/99-scurry-dongle.rules
              cp ${./scurry.toml.example} scurry/scurry.toml.example
              cp ${./assets/icon-256.png} scurry/scurry.png
              tar -czf $out scurry
            '';

          appimage = nix-appimage.lib.${system}.mkAppImage {
            program = "${scurry-tray}/bin/scurry-tray";
            pname = "scurry-tray";
            name = "scurry-tray-${if system == "x86_64-linux" then "x86_64" else "aarch64"}.AppImage";
          };
        }
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isDarwin {
          release =
            let arch = if system == "x86_64-darwin" then "amd64" else "arm64";
            in pkgs.runCommand "scurry-macos-${arch}.dmg"
              { nativeBuildInputs = [ pkgs.cctools ]; } ''
              # hdiutil is a system tool with no nixpkgs equivalent, and the
              # system otool/install_name_tool must match the linker that
              # produced these binaries.
              export PATH="/usr/bin:$PATH"
              mkdir -p staging

              cp -rL "${scurry-tray}/Applications/scurry.app" staging/
              chmod -R u+w staging/
              # The daemon ships inside the bundle so the launchd plist has a
              # stable path to point at, and so there is one thing to drag.
              cp ${scurry-ctl}/bin/scurry-ctl "staging/scurry.app/Contents/MacOS/"

              for bin in staging/scurry.app/Contents/MacOS/scurry-tray \
                         staging/scurry.app/Contents/MacOS/scurry-ctl; do
                chmod +w "$bin"
                # Rewrite /nix/store dylib references to /usr/lib so the binary
                # resolves its libraries on a Mac that has never seen Nix.
                for dep in $(otool -L "$bin" | grep /nix/store | awk '{print $1}'); do
                  install_name_tool -change "$dep" "/usr/lib/$(basename "$dep")" "$bin"
                done

                # Fail the build if anything survived. A /nix/store
                # LC_LOAD_DYLIB or LC_RPATH makes dyld abort on a user's Mac,
                # and catching that here costs far less than catching it from a
                # bug report about an app that will not open.
                if otool -L "$bin" | grep -q '/nix/store'; then
                  echo "ERROR: $bin still references /nix/store dylibs:" >&2
                  otool -L "$bin" | grep '/nix/store' >&2
                  exit 1
                fi
                if otool -l "$bin" | grep -A2 LC_RPATH | grep -q '/nix/store'; then
                  echo "ERROR: $bin has /nix/store in LC_RPATH:" >&2
                  otool -l "$bin" | grep -A2 LC_RPATH >&2
                  exit 1
                fi
              done

              # Destinations are named explicitly: `cp \''${./x} dir/` keeps the
              # store hash in the filename, and Install Daemon.command looks the
              # plist up by name.
              cp ${./packaging/com.ananthb.scurry.plist} staging/com.ananthb.scurry.plist
              cp ${./scurry.toml.example} staging/scurry.toml.example

              cat > "staging/Install Daemon.command" <<'SCRIPT'
              #!/bin/bash
              # Installs the LaunchAgent so the daemon starts at login.
              set -e
              app="/Applications/scurry.app"
              if [ ! -d "$app" ]; then
                echo "Drag scurry.app to Applications first, then run this again."
                exit 1
              fi
              plist="$HOME/Library/LaunchAgents/com.ananthb.scurry.plist"
              mkdir -p "$HOME/Library/LaunchAgents"
              sed "s|HOME_DIR|$HOME|g" "$(dirname "$0")/com.ananthb.scurry.plist" > "$plist"
              launchctl bootout "gui/$(id -u)/com.ananthb.scurry" 2>/dev/null || true
              launchctl bootstrap "gui/$(id -u)" "$plist"
              echo "Installed and started. Logs: ~/Library/Logs/scurry.log"
              echo
              echo "scurry needs Accessibility permission to capture the pointer:"
              echo "System Settings > Privacy & Security > Accessibility > add scurry."
              SCRIPT
              chmod +x "staging/Install Daemon.command"

              ln -s /Applications staging/Applications

              # `hdiutil create -srcfolder` intermittently fails with "Resource
              # busy" on GitHub macOS runners: its internal attach/convert step
              # races with mds and the runner agent touching the staging dir. A
              # short backoff covers it.
              attempt=1
              until hdiutil create -volname "scurry" -srcfolder staging \
                -ov -format UDZO "$out"; do
                if [ "$attempt" -ge 3 ]; then
                  echo "hdiutil create failed after $attempt attempts" >&2
                  exit 1
                fi
                echo "hdiutil create failed (attempt $attempt), retrying..." >&2
                rm -f "$out"
                sleep $((attempt * 5))
                attempt=$((attempt + 1))
              done
            '';
        };

        checks.workspace = pkgs.rustPlatform.buildRustPackage {
          pname = "scurry-checks";
          version = cargoVersion;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          inherit nativeBuildInputs buildInputs;
          buildPhase = "cargo clippy --workspace --all-targets -- -D warnings";
          checkPhase = "cargo test --workspace";
          installPhase = "touch $out";
        };

        devShells = {
          default = pkgs.mkShell {
            inherit nativeBuildInputs;
            buildInputs = buildInputs ++ [ rustToolchain pkgs.espflash pkgs.esptool ];
            # eframe dlopens libGL and the Wayland/X11 client libraries at
            # runtime rather than linking them, so they must be on the loader
            # path even though the build succeeded without them.
            LD_LIBRARY_PATH = lib.makeLibraryPath buildInputs;
          };

          # ESP-IDF comes from nixpkgs-esp-dev's own package set, not via its
          # overlay applied to ours: the overlay wants python310, which unstable
          # has dropped. Kept out of the default shell because the IDF closure is
          # large and the host crates never need it.
          esp = pkgs.mkShell {
            packages = [
              nixpkgs-esp-dev.packages.${system}.esp-idf-riscv
              pkgs.espflash
              # The firmware build shells out to cargo to compile the Rust
              # layout engine for riscv32imc.
              rustToolchain
            ];
          };
        };
      });
}
