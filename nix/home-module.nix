# home-manager module for scurry.
#
# scurry is a single process: the tray owns the dongle's serial port, captures
# input, and shows the menu. So there is one service here, not a daemon and a
# GUI, and there is no configuration file -- the virtual desktop lives on the
# dongle itself, which is what lets it survive moving to another controller.
{ config, lib, pkgs, ... }:

let
  cfg = config.services.scurry;
in
{
  options.services.scurry = {
    enable = lib.mkEnableOption "scurry, sharing one mouse and keyboard across machines";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The scurry-tray package to use.";
    };

    ctlPackage = lib.mkOption {
      type = lib.types.package;
      description = "The scurry-ctl package to use, for the command line.";
    };

    autostart = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Start scurry at login.

        The app can also manage this itself from its "Open at Login" menu item.
        Leaving this enabled makes the login item declarative instead, so it is
        rebuilt from configuration rather than from whatever was last clicked;
        the menu toggle then has nothing to add.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package cfg.ctlPackage ];

    # macOS: a LaunchAgent, which is also how the app's own menu toggle would
    # register itself. Declaring it here means the same file is managed by the
    # generation rather than written at runtime.
    # Copy the .app into ~/Applications on activation.
    #
    # home.file with recursive = true would make per-file symlinks, which macOS
    # does not accept as a bundle. Copy the whole thing.
    #
    # This is also what gives scurry a stable identity for Accessibility: macOS
    # grants that permission per binary, and a bare binary under
    # /etc/profiles/per-user changes path with every generation, so the grant
    # would be silently lost on each rebuild.
    home.activation.scurry-app = lib.mkIf pkgs.stdenv.hostPlatform.isDarwin
      (lib.hm.dag.entryAfter [ "writeBoundary" ] ''
        app_src="${cfg.package}/Applications/scurry.app"
        app_dst="$HOME/Applications/scurry.app"
        stamp="$app_dst/Contents/.scurry-store-path"
        if [ -d "$app_src" ]; then
          # Do nothing at all when scurry has not changed.
          #
          # This ran on every generation, and most generations have nothing to
          # do with scurry -- adding a shell alias rebuilt and replaced the app
          # too. Each time it deleted the bundle and made a new one, and
          # deleting an app is how you tell macOS to forget it: TCC prunes the
          # Accessibility grant for a bundle that is no longer there, so the
          # permission had to be granted again after a rebuild that changed
          # nothing about scurry whatsoever.
          if [ -f "$stamp" ] && [ "$(cat "$stamp" 2>/dev/null)" = "${cfg.package}" ]; then
            : # already current
          else
          # rsync into the existing directory rather than rm -rf and cp.
          #
          # The bundle keeps its identity on disk and only its contents change,
          # so there is never a moment when ~/Applications/scurry.app does not
          # exist. The signature below is what makes the grant survive the new
          # binary; not deleting the bundle is what stops it being thrown away
          # before the signature gets a chance to.
          $DRY_RUN_CMD mkdir -p "$app_dst"
          $DRY_RUN_CMD ${pkgs.rsync}/bin/rsync -a --delete --copy-links \
            "$app_src/" "$app_dst/"
          $DRY_RUN_CMD chmod -R u+w "$app_dst"
          $DRY_RUN_CMD xattr -dr com.apple.quarantine "$app_dst" 2>/dev/null || true

          # Sign the bundle, not just the executable.
          #
          # Rust's linker leaves an ad-hoc signature on the binary alone, with
          # the Info.plist unbound -- so codesign reports Identifier=scurry-tray
          # rather than com.ananthb.scurry-tray, and macOS sees a bare
          # executable rather than an app with a bundle identity. Accessibility
          # is granted against that identity, so without signing the bundle the
          # permission cannot stick however many times it is granted.
          #
          # Prefer a local certificate over an ad-hoc signature. Ad-hoc is keyed
          # by cdhash, so every rebuild is a new identity and the grant is
          # silently revoked; a certificate makes the designated requirement
          # depend on the bundle id and the certificate instead, so it survives.
          # Create one with nix/setup-signing.sh.
          if /usr/bin/security find-identity -v -p codesigning 2>/dev/null \
              | grep -q scurry-local-signing; then
            # Failures are reported rather than swallowed. A silent `|| true`
            # here left the bundle carrying the ad-hoc signature the linker
            # produced, which looks identical until the permission is quietly
            # dropped on the next rebuild -- the exact symptom this signing
            # exists to prevent, hidden by the error handling meant to be
            # tidy about it.
            $DRY_RUN_CMD /usr/bin/codesign --force --deep \
              --sign scurry-local-signing "$app_dst" >/dev/null 2>&1 \
              || echo "scurry: signing the app bundle failed; Accessibility" \
                      "will need re-granting after each rebuild" >&2
          else
            echo "scurry: no local signing identity; Accessibility will need" \
                 "re-granting after each rebuild. Run nix/setup-signing.sh." >&2
            $DRY_RUN_CMD /usr/bin/codesign --force --deep --sign - "$app_dst" \
              >/dev/null 2>&1 || true
          fi

          # Written last, so an interrupted or failed install is not recorded
          # as current and is retried on the next generation.
          $DRY_RUN_CMD printf '%s' "${cfg.package}" > "$stamp"
          fi
        fi
      '');

    launchd.agents.scurry = lib.mkIf (cfg.autostart && pkgs.stdenv.hostPlatform.isDarwin) {
      enable = true;
      config = {
        # Set explicitly. Without it home-manager derives the label from the
        # attribute name -- org.nix-community.home.scurry -- which would not
        # match the com.ananthb.scurry the app's own "Open at Login" item writes
        # and that `launchctl bootout` targets. Two labels for one agent means
        # the app cannot see or stop what home-manager installed.
        Label = "com.ananthb.scurry";
        # Launched from the bundle, not from the store path, so the process has
        # the app's identity. Accessibility is granted per binary, and a store
        # path changes on every rebuild -- the permission would have to be
        # re-granted each time.
        ProgramArguments = [
          "${config.home.homeDirectory}/Applications/scurry.app/Contents/MacOS/scurry-tray"
        ];
        RunAtLoad = true;
        # No KeepAlive. This is a UI app: relaunching it after the user quits
        # from the menu would make it impossible to stop.
        ProcessType = "Interactive";
        StandardErrorPath = "${config.home.homeDirectory}/Library/Logs/scurry.log";
        StandardOutPath = "${config.home.homeDirectory}/Library/Logs/scurry.log";
      };
    };

    systemd.user.services.scurry = lib.mkIf (cfg.autostart && pkgs.stdenv.hostPlatform.isLinux) {
      Unit = {
        Description = "scurry input sharing";
        Documentation = [ "https://github.com/ananthb/scurry" ];
        After = [ "graphical-session-pre.target" ];
        PartOf = [ "graphical-session.target" ];
      };

      Service = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/scurry-tray";
        Restart = "on-failure";
        RestartSec = 5;
      };

      Install.WantedBy = [ "graphical-session.target" ];
    };

    # Reuses the packaged entry so the launcher shows the same name and comment
    # the distro packages do; only the binary path differs.
    xdg.configFile."autostart/scurry-tray.desktop" =
      lib.mkIf (cfg.autostart && pkgs.stdenv.hostPlatform.isLinux) {
        text = builtins.replaceStrings
          [ "Exec=scurry-tray" ]
          [ "Exec=${cfg.package}/bin/scurry-tray" ]
          (builtins.readFile ../packaging/scurry-tray.desktop);
      };

    warnings = lib.optional (cfg.enable && pkgs.stdenv.hostPlatform.isLinux) ''
      scurry needs read/write access to the dongle's USB serial device. This is
      a system-level permission that home-manager cannot grant: install
      packaging/99-scurry-dongle.rules to /etc/udev/rules.d, or add yourself to
      the dialout group. Without it scurry will find no dongle.
    '';
  };
}
