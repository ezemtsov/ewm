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

  applySkia = emacs: emacs.overrideAttrs (o: {
    patches = (o.patches or []) ++ [
      ../emacs/emacs-30.2-pgtk-skia-sources.patch
      ../emacs/emacs-30.2-pgtk-skia-integration.patch
    ];
    configureFlags = (o.configureFlags or []) ++ [ "--with-skia" ];
    buildInputs = (o.buildInputs or []) ++ (with pkgs; [ skia libepoxy ]);
    nativeBuildInputs = (o.nativeBuildInputs or []) ++ (with pkgs; [ autoconf automake ]);
    preConfigure = (o.preConfigure or "") + ''
      ./autogen.sh
    '';
  });

  # Rewrap a withPackages result so its base emacs is skia-patched; on a
  # bare emacs, patch it directly. withPackages results carry `emacs` and
  # `explicitRequires` but no `.override`, so rebuild via overrideScope.
  wrapSkia = pkg:
    if !cfg.withSkia then pkg
    else if pkg ? emacs && pkg ? explicitRequires then
      ((pkgs.emacsPackagesFor pkg.emacs).overrideScope (_: _: {
        emacs = applySkia pkg.emacs;
      })).emacsWithPackages (_: pkg.explicitRequires)
    else applySkia pkg;

  ewmPackage = pkgs.callPackage ./default.nix {
    withScreencastSupport = cfg.screencast.enable;
    emacsPackage = cfg.emacsPackage.emacs or cfg.emacsPackage;
  };

  # Launch script that runs EWM in the provided Emacs.
  launchScript = pkgs.writeShellScript "ewm-launch" ''
    ${lib.optionalString (cfg.renderDevice != null) ''
      export EWM_RENDER_DEVICE=${lib.escapeShellArg cfg.renderDevice}
    ''}
    exec ${cfg.emacsPackage}/bin/emacs \
      --fg-daemon \
      --eval "(require 'ewm)" \
      --eval "(ewm-start-module)" \
      ${cfg.extraEmacsArgs} "$@"
  '';

  ewmSystemPackage = pkgs.runCommand "ewm-system" {
    passthru.providedSessions = [ "ewm" ];
  } ''
    install -Dm755 ${launchScript} $out/bin/ewm-launch
    install -Dm755 ${../resources/ewm-session} $out/bin/ewm-session
    install -Dm644 ${../resources/ewm.desktop} $out/share/wayland-sessions/ewm.desktop
    mkdir -p $out/lib/systemd/user
    substitute ${../resources/ewm.service} $out/lib/systemd/user/ewm.service \
      --replace-fail 'ExecStart=ewm-launch' 'ExecStart=/run/current-system/sw/bin/ewm-launch'
    install -Dm644 ${../resources/ewm-shutdown.target} $out/lib/systemd/user/ewm-shutdown.target
    install -Dm644 ${../resources/ewm-portals.conf} $out/share/xdg-desktop-portal/ewm-portals.conf
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

    withSkia = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Enable Skia backend for Emacs.";
    };

    emacsPackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.emacs-pgtk.pkgs.withPackages (_: [ ewmPackage ]);
      description = "Emacs package to use. Must be a pgtk build with the EWM package available.";
      example = "pkgs.emacs30-pgtk";
      apply = wrapSkia;
    };

    ewmPackage = lib.mkOption {
      type = lib.types.package;
      default = ewmPackage;
      description = "EWM package to use.";
    };

    extraEmacsArgs = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Verbatim Emacs arguments to add to the launch flags.";
      example = "--no-site-lisp --eval '(foo)'";
    };

    renderDevice = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "DRM render device path for GPU selection (e.g. /dev/dri/renderD129). When null, the primary GPU is auto-detected.";
      example = "/dev/dri/renderD129";
    };

    screencast.enable = lib.mkEnableOption "screen casting via PipeWire" // {
      default = true;
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ ewmSystemPackage ];
    services.displayManager.sessionPackages = [ ewmSystemPackage ];
    systemd.packages = [ ewmSystemPackage ];
    security.polkit.enable = true;

    # Provides fonts, xdg-utils, graphics, PipeWire, inotify limits, etc.
    services.graphical-desktop.enable = true;

    services.pipewire.wireplumber.enable = lib.mkIf cfg.screencast.enable (lib.mkDefault true);

    environment.sessionVariables = {
      NIXOS_OZONE_WL = lib.mkDefault 1;
    };

    services.gnome.gnome-keyring.enable = lib.mkDefault true;

    # XDG portal configuration for screen sharing, file dialogs, etc.
    xdg.portal = {
      enable = lib.mkDefault true;
      configPackages = [ ewmSystemPackage ];
      extraPortals = [ pkgs.xdg-desktop-portal-gnome pkgs.xdg-desktop-portal-gtk ];
    };
  };
}
