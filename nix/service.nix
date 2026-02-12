# EWM NixOS Module
#
# Usage in /etc/nixos/configuration.nix:
#   imports = [ /path/to/ewm/service.nix ];
#   programs.ewm.enable = true;
#
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.ewm;

  ewmPackage = pkgs.callPackage ./default.nix {
    withScreencastSupport = cfg.screencast.enable;
  };

  # Session script that launches Emacs with EWM
  initDirArg = lib.optionalString (cfg.initDirectory != null)
    "--init-directory ${cfg.initDirectory}";

  ewmSession = pkgs.writeShellScriptBin "ewm-session" ''
    export EWM_MODULE_PATH="${ewmPackage}/lib/ewm/libewm_core.so"
    export XDG_CURRENT_DESKTOP=ewm
    export XDG_SESSION_TYPE=wayland
    export EMACSLOADPATH="${ewmPackage}/share/emacs/site-lisp/ewm:$EMACSLOADPATH"

    # Import environment into systemd user session
    systemctl --user import-environment

    # Update dbus activation environment
    if command -v dbus-update-activation-environment >/dev/null 2>&1; then
      dbus-update-activation-environment --all
    fi

    # Start Emacs as foreground daemon and initialize EWM
    exec ${cfg.emacsPackage}/bin/emacs --fg-daemon ${initDirArg} \
      --eval "(require 'ewm)" \
      --eval "(ewm-start-module)" \
      "$@"
  '';

  # Wayland session file for display managers
  sessionPackage = pkgs.runCommand "ewm-session" {
    passthru.providedSessions = [ "ewm" ];
  } ''
    mkdir -p $out/share/wayland-sessions
    cat > $out/share/wayland-sessions/ewm.desktop << EOF
    [Desktop Entry]
    Name=EWM
    Comment=Emacs Wayland Manager
    Exec=${ewmSession}/bin/ewm-session
    Type=Application
    DesktopNames=ewm
    EOF
  '';

in
{
  options.programs.ewm = {
    enable = lib.mkEnableOption "EWM, an Emacs Wayland Manager";

    package = lib.mkOption {
      type = lib.types.package;
      default = ewmPackage;
      description = "The EWM package to use.";
    };

    emacsPackage = lib.mkPackageOption pkgs "emacs" { };

    initDirectory = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Emacs init directory (passed as --init-directory).";
      example = "/etc/nixos/dotfiles/emacs";
    };

    screencast.enable = lib.mkEnableOption "screen casting via PipeWire" // {
      default = true;
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [
      cfg.package
      ewmSession
    ];

    services.displayManager.sessionPackages = [ sessionPackage ];

    security.polkit.enable = true;

    services.pipewire = lib.mkIf cfg.screencast.enable {
      enable = lib.mkDefault true;
      wireplumber.enable = lib.mkDefault true;
    };

    services.dbus.enable = true;

    environment.sessionVariables = {
      MOZ_ENABLE_WAYLAND = "1";
    };
  };
}
