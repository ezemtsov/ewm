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
    inherit (cfg) emacsPackage initDirectory;
  };

  # Session script that sets up environment and starts ewm.service
  ewmSession = pkgs.writeShellScriptBin "ewm-session" ''
    # Re-exec through a login shell to get the full NixOS environment
    # (PATH, etc.) before importing it into systemd. Without this,
    # the display manager starts us with a minimal PATH.
    if [ -n "$SHELL" ] &&
       grep -q "$SHELL" /etc/shells &&
       ! (echo "$SHELL" | grep -q "false") &&
       ! (echo "$SHELL" | grep -q "nologin"); then
      if [ "$1" != '-l' ]; then
        exec bash -c "exec -l '$SHELL' -c '$0 -l $*'"
      else
        shift
      fi
    fi

    # Check for existing session
    if systemctl --user -q is-active ewm.service; then
      echo 'An EWM session is already running.'
      exit 1
    fi

    # Reset failed state of user units
    systemctl --user reset-failed

    # Import the login manager environment to systemd
    systemctl --user import-environment

    # Update D-Bus activation environment
    if command -v dbus-update-activation-environment >/dev/null 2>&1; then
      dbus-update-activation-environment --all
    fi

    # Start EWM and wait for it to terminate
    systemctl --user --wait start ewm.service

    # Force stop of graphical-session.target on exit
    systemctl --user start --job-mode=replace-irreversibly ewm-shutdown.target

    # Clean up environment
    systemctl --user unset-environment WAYLAND_DISPLAY XDG_SESSION_TYPE XDG_CURRENT_DESKTOP
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

    emacsPackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.emacs-pgtk;
      description = "Emacs package to use. Must be a pgtk build for Wayland support.";
      example = "pkgs.emacs30-pgtk";
    };

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

    # Required for DRM backend (provides libEGL, mesa drivers)
    hardware.graphics.enable = lib.mkDefault true;

    services.pipewire = lib.mkIf cfg.screencast.enable {
      enable = lib.mkDefault true;
      wireplumber.enable = lib.mkDefault true;
    };

    services.dbus.enable = true;

    environment.sessionVariables = {
      MOZ_ENABLE_WAYLAND = "1";
    };

    # Recommended for Wayland compositors
    programs.dconf.enable = lib.mkDefault true;
    services.gnome.gnome-keyring.enable = lib.mkDefault true;

    # XDG portal configuration for screen sharing, file dialogs, etc.
    xdg.portal = {
      enable = lib.mkDefault true;
      wlr.enable = lib.mkDefault true;
      extraPortals = [ pkgs.xdg-desktop-portal-gtk ];
      # Portal configuration for EWM
      config.ewm = {
        default = [ "gtk" ];
        "org.freedesktop.impl.portal.ScreenCast" = "wlr";
        "org.freedesktop.impl.portal.Screenshot" = "wlr";
        # Inhibit portal doesn't work with wlr, use none to avoid issues
        "org.freedesktop.impl.portal.Inhibit" = "none";
      };
    };

    # Run XDG autostart files (window managers don't do this automatically)
    services.xserver.desktopManager.runXdgAutostartIfNone = lib.mkDefault true;
  };
}
