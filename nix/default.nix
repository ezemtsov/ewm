{ pkgs ? import <nixpkgs> {}
, withScreencastSupport ? true
, emacsPackage ? pkgs.emacs-pgtk
, initDirectory ? null
}:

let
  inherit (pkgs) lib;
  inherit (pkgs) rustPlatform pkg-config;
  inherit (pkgs) libdrm libgbm libglvnd libinput libxkbcommon pipewire seatd systemd wayland;

  # Rust compositor - only rebuilds when compositor/ changes
  ewm-compositor = rustPlatform.buildRustPackage {
    pname = "ewm-compositor";
    version = "0.1.0";

    src = ./../compositor;

    cargoLock = {
      lockFile = ./../compositor/Cargo.lock;
      outputHashes = {
        "smithay-0.4.0" = "sha256-M7fv3Y54cMv6uQcyVtt984AKxIIgfHJZidQnSLZ/C7o=";
        "pipewire-0.8.0" = "sha256-kp5x5QhmgEqCrt7xDRfMFGoTK5IXOuvW2yOW02B8Ftk=";
      };
    };

    strictDeps = true;

    nativeBuildInputs = [
      pkg-config
      rustPlatform.bindgenHook
    ];

    buildInputs = [
      libdrm
      libgbm
      libglvnd # For libEGL
      libinput
      libxkbcommon
      seatd
      systemd # For libudev
      wayland
    ] ++ lib.optional withScreencastSupport pipewire;

    buildFeatures = lib.optional withScreencastSupport "screencast";
    buildNoDefaultFeatures = true;

    env = {
      # Force linking with libEGL and libwayland-client
      # so they can be discovered by dlopen()
      RUSTFLAGS = toString (
        map (arg: "-C link-arg=" + arg) [
          "-Wl,--push-state,--no-as-needed"
          "-lEGL"
          "-lwayland-client"
          "-Wl,--pop-state"
        ]
      );
    };

    postInstall = ''
      install -Dm0755 $releaseDir/libewm_core.so $out/lib/ewm/libewm_core.so
    '';

    doCheck = false;
  };

  # Emacs lisp package - only rebuilds when lisp/ changes
  ewm-lisp = pkgs.stdenv.mkDerivation {
    pname = "ewm-lisp";
    version = "0.1.0";

    src = ./../lisp;

    installPhase = ''
      install -Dm0644 *.el -t $out/share/emacs/site-lisp/ewm
    '';
  };

  initDirArg = lib.optionalString (initDirectory != null)
    "--init-directory ${initDirectory}";

  # Wrapper script for launching Emacs with EWM
  ewm-emacs = pkgs.writeShellScript "ewm-emacs" ''
    export EWM_MODULE_PATH="${ewm-compositor}/lib/ewm/libewm_core.so"
    export EMACSLOADPATH="${ewm-lisp}/share/emacs/site-lisp/ewm:$EMACSLOADPATH"

    exec ${emacsPackage}/bin/emacs --fg-daemon ${initDirArg} \
      --eval "(require 'ewm)" \
      --eval "(ewm-start-module)" \
      "$@"
  '';

  resources = ./../resources;

  # Wrapper + systemd units - rebuilds when emacsPackage or initDirectory change
  ewm-session = pkgs.runCommand "ewm-session" {} ''
    # Install wrapper script
    install -Dm0755 ${ewm-emacs} $out/bin/ewm-emacs

    # Install systemd unit files
    install -Dm0644 ${resources}/ewm.service -t $out/share/systemd/user/
    install -Dm0644 ${resources}/ewm-shutdown.target -t $out/share/systemd/user/

    # Patch ExecStart to point to the nix store wrapper
    substituteInPlace $out/share/systemd/user/ewm.service \
      --replace-fail "/usr/bin/ewm-emacs" "$out/bin/ewm-emacs"
  '';

in
# Combined package
pkgs.symlinkJoin {
  name = "ewm-0.1.0";

  paths = [
    ewm-compositor
    ewm-lisp
    ewm-session
  ];

  passthru = {
    inherit ewm-compositor ewm-lisp ewm-session;
    emacsLoadPath = "/share/emacs/site-lisp/ewm";
  };

  meta = {
    description = "Emacs Wayland Manager - Wayland compositor for Emacs";
    homepage = "https://github.com/ezemtsov/ewm";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.linux;
    mainProgram = "ewm-emacs";
  };
}
