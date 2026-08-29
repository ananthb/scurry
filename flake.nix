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

        # Nightly, for the firmware only: its std target riscv32imc-esp-espidf
        # is tier 3, so rustc knows the target spec but ships no std for it and
        # it must be built from source with -Zbuild-std.
        rustToolchain = pkgs.rust-bin.nightly.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
          targets = [ "riscv32imc-unknown-none-elf" ];
        };

        # Stable for everything shipped. Release binaries have no reason to ride
        # on nightly just because the firmware does.
        hostToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "clippy" "rustfmt" "rust-src" ];
        };

        # buildRustPackage otherwise uses nixpkgs' own rustc and cargo, which do
        # not carry the extensions requested above -- so `cargo clippy` in the
        # checks derivation failed with "no such command", despite clippy being
        # named right there in the toolchain.
        hostRustPlatform = pkgs.makeRustPlatform {
          cargo = hostToolchain;
          rustc = hostToolchain;
        };

        # Needed at build and run time on Linux. Missing any of them shows up as
        # a link error, or worse as a binary that builds and then cannot open a
        # window on the user's machine.
        linuxDeps = with pkgs; [
          # serialport enumerates ports through libudev on Linux, so this is
          # needed by the daemon as well as the GUI -- without it libudev-sys
          # fails at build time, not at run time.
          systemdLibs
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
        buildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux linuxDeps;

        mkScurry = { pname, cargoBuildFlags }: hostRustPlatform.buildRustPackage {
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
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux (
          let
            arch = if system == "x86_64-linux" then "amd64" else "arm64";

            # Nix bakes an RPATH into /nix/store. On a machine without Nix the
            # loader follows it, finds nothing, and the binary dies before main.
            # Stripping it makes these behave like any other distro binary.
            # Shared by the tarball and the packages so neither can be shipped
            # with the RPATH still in it.
            portableBins = pkgs.runCommand "scurry-bins-${arch}"
              { nativeBuildInputs = [ pkgs.patchelf ]; } ''
              mkdir -p $out
              cp ${scurry-ctl}/bin/scurry-ctl $out/
              cp ${scurry-tray}/bin/scurry-tray $out/
              chmod +w $out/scurry-ctl $out/scurry-tray
              patchelf --remove-rpath $out/scurry-ctl
              patchelf --remove-rpath $out/scurry-tray
              if readelf -d $out/scurry-tray | grep -q 'R\(UN\)\?PATH'; then
                echo "ERROR: RPATH survived in scurry-tray" >&2
                readelf -d $out/scurry-tray | grep 'R\(UN\)\?PATH' >&2
                exit 1
              fi
            '';

            # One nfpm config, built twice. nfpm maps the arch names itself, so
            # deb gets amd64/arm64 and rpm gets x86_64/aarch64 without us
            # spelling out either.
            nfpmConfig = pkgs.writeText "nfpm.yaml" ''
              name: scurry
              arch: ${arch}
              platform: linux
              version: ${cargoVersion}
              section: utils
              priority: optional
              maintainer: Ananth Bhaskararaman <antsub@gmail.com>
              description: |
                Share one mouse and keyboard across machines.
                The pointer crosses a screen edge and lands on the next machine.
                Targets need no software: they see an ordinary Bluetooth mouse.
              vendor: ananthb
              homepage: https://github.com/ananthb/scurry
              license: MIT
              contents:
                - src: ${portableBins}/scurry-ctl
                  dst: /usr/bin/scurry-ctl
                - src: ${portableBins}/scurry-tray
                  dst: /usr/bin/scurry-tray
                - src: ${./packaging/scurry-tray.desktop}
                  dst: /usr/share/applications/scurry-tray.desktop
                - src: ${./assets/icon-256.png}
                  dst: /usr/share/icons/hicolor/256x256/apps/scurry.png
                - src: ${./assets/icon-128.png}
                  dst: /usr/share/icons/hicolor/128x128/apps/scurry.png
                # Without this the dongle is root-only on most distributions and
                # the app silently finds nothing to talk to.
                - src: ${./packaging/99-scurry-dongle.rules}
                  dst: /usr/lib/udev/rules.d/99-scurry-dongle.rules
            '';

            mkPackage = format: ext: pkgs.runCommand "scurry-${arch}.${ext}"
              { nativeBuildInputs = [ pkgs.nfpm ]; } ''
              mkdir -p out
              nfpm package --config ${nfpmConfig} --packager ${format} --target out/
              mv out/* $out
            '';
          in
          {
            release = pkgs.runCommand "scurry-${arch}.tar.gz"
              { nativeBuildInputs = [ pkgs.gzip ]; } ''
              mkdir -p scurry
              cp ${portableBins}/scurry-ctl ${portableBins}/scurry-tray scurry/
              cp ${./packaging/scurry-tray.desktop} scurry/scurry-tray.desktop
              cp ${./packaging/99-scurry-dongle.rules} scurry/99-scurry-dongle.rules
              cp ${./scurry.toml.example} scurry/scurry.toml.example
              cp ${./assets/icon-256.png} scurry/scurry.png
              tar -czf $out scurry
            '';

            deb = mkPackage "deb" "deb";
            rpm = mkPackage "rpm" "rpm";

            appimage = nix-appimage.lib.${system}.mkAppImage {
              program = "${scurry-tray}/bin/scurry-tray";
              pname = "scurry-tray";
              name = "scurry-tray-${if system == "x86_64-linux" then "x86_64" else "aarch64"}.AppImage";
            };
          })
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
              # Nothing else ships. The app registers its own login item from the
              # menu and prompts for Accessibility with the system's own dialog,
              # so there is no installer script and nothing for the user to read:
              # drag it across and open it.
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

        checks.workspace = hostRustPlatform.buildRustPackage {
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
