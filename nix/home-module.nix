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
    launchd.agents.scurry = lib.mkIf (cfg.autostart && pkgs.stdenv.hostPlatform.isDarwin) {
      enable = true;
      config = {
        ProgramArguments = [ "${cfg.package}/bin/scurry-tray" ];
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
