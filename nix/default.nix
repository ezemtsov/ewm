{ pkgs ? import <nixpkgs> {}
, withScreencastSupport ? true
}:

let
  inherit (pkgs) lib;
  inherit (pkgs) rustPlatform pkg-config;
  inherit (pkgs) libdrm libgbm libglvnd libinput libxkbcommon pipewire seatd systemd wayland;
in

rustPlatform.buildRustPackage {
  pname = "ewm";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = builtins.fetchGit {
      url = ./..;
      ref = "HEAD";
    };
    filter = path: type:
      (baseNameOf path) != "nix";
  };

  sourceRoot = "source/compositor";

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
    # Install the dynamic library
    install -Dm0755 $releaseDir/libewm_core.so $out/lib/ewm/libewm_core.so

    # Install Emacs lisp files
    install -Dm0644 ../lisp/*.el -t $out/share/emacs/site-lisp/ewm
  '';

  # Skip tests as they require a display
  doCheck = false;

  passthru = {
    # Helper for Emacs to find the module
    emacsLoadPath = "/share/emacs/site-lisp/ewm";
  };

  meta = {
    description = "Emacs Wayland Manager - Wayland compositor for Emacs";
    homepage = "https://github.com/ezemtsov/ewm";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.linux;
    mainProgram = null; # Library, not executable
  };
}
