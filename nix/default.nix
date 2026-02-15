{ pkgs ? import <nixpkgs> {}
, withScreencastSupport ? true
, emacsPackage ? pkgs.emacs-pgtk
}:

let
  inherit (pkgs) lib;
  inherit (pkgs) rustPlatform pkg-config;
  inherit (pkgs) libdrm libgbm libglvnd libinput libxkbcommon pipewire seatd systemd wayland;

  # Rust compositor core - only rebuilds when compositor/ changes
  ewm-core = rustPlatform.buildRustPackage {
    pname = "ewm-core";
    version = "0.1.0";

    src = lib.cleanSource ./../compositor;

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
      mkdir -p $out/share/emacs/site-lisp
      ln -s $out/lib/libewm_core.so $out/share/emacs/site-lisp/ewm-core.so
    '';

    doCheck = false;
  };

in
emacsPackage.pkgs.trivialBuild {
  pname = "ewm";
  version = "0.1.0";
  src = lib.cleanSource ./../lisp;
  packageRequires = [ ewm-core ];
  passthru.module = "${./service.nix}";

  meta = {
    description = "Emacs Wayland Manager - Wayland compositor for Emacs";
    homepage = "https://github.com/ezemtsov/ewm";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.linux;
    mainProgram = "ewm-emacs";
  };
}
