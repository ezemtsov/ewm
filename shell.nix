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
    glib.dev # For GIO (XDG app enumeration)

    # For winit backend
    libx11
    libxcursor
    libxrandr
    libxi

    # For DRM/libinput backend (standalone session)
    seatd.dev
    libinput.dev
    systemd.dev  # provides libudev.pc
    libdrm.dev
    libgbm       # provides gbm

    # For PipeWire screen sharing
    pipewire.dev
    llvmPackages.libclang.lib  # for bindgen

    # Wayland debugging utilities
    grim
    wf-recorder    # screen recording (uses wlr-screencopy)
    wlr-randr      # output configuration
    wayland-utils  # wayland-info
    wev            # wayland event viewer
    slurp          # region selection
    ffmpeg         # for video inspection
  ];

  LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
  BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include";

  LD_LIBRARY_PATH = with pkgs; lib.makeLibraryPath [
    libxkbcommon
    libGL
    wayland
    libx11
    libxcursor
    libxrandr
    libxi
    seatd
    libinput
    systemd  # libudev runtime
    libdrm
    libgbm   # gbm runtime
  ];
}
