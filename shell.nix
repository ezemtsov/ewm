{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    pkg-config
    cargo
    rustc
    rust-analyzer
  ];

  buildInputs = with pkgs; [
    # Smithay dependencies
    libxkbcommon
    libGL
    wayland

    # For winit backend
    xorg.libX11
    xorg.libXcursor
    xorg.libXrandr
    xorg.libXi

    # For DRM/libinput backend (standalone session)
    seatd.dev
    libinput.dev
    systemd.dev  # provides libudev.pc
    libdrm.dev
    libgbm       # provides gbm

    # For tests
    grim
  ];

  LD_LIBRARY_PATH = with pkgs; lib.makeLibraryPath [
    libxkbcommon
    libGL
    wayland
    xorg.libX11
    xorg.libXcursor
    xorg.libXrandr
    xorg.libXi
    seatd
    libinput
    systemd  # libudev runtime
    libdrm
    libgbm   # gbm runtime
  ];
}
