{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    autoconf
    automake
    clang-tools
    gdb
    pkg-config
    texinfo

    # Smoke-test helpers.
    grim
    (python3.withPackages (ps: [ ps.pillow ]))
  ];

  buildInputs = with pkgs; [
    # Skia.
    skia
    fontconfig
    freetype
    harfbuzz
    icu
    libepoxy
    libGL
    libGLU
    libx11
    libxext

    # Image formats and compression.
    expat
    giflib
    libjpeg
    libpng
    librsvg
    libtiff
    libwebp
    zlib

    # Emacs build/runtime dependencies.
    gdk-pixbuf
    glib
    gnutls
    gtk3
    jansson
    libxml2
    ncurses
    ncurses.dev
    pango
    sqlite
    tree-sitter
    webkitgtk_4_1
  ];

  shellHook = ''
    export SKIA_DIR="${pkgs.skia}"
    export SKIA_LIBS="-L${pkgs.skia}/lib -lskia"
    export SKIA_CFLAGS="-I${pkgs.skia}/include/skia"

    echo "Emacs Skia development environment ready"
    echo "SKIA_DIR=$SKIA_DIR"
    echo "SKIA_LIBS=$SKIA_LIBS"
    echo "SKIA_CFLAGS=$SKIA_CFLAGS"
  '';
}
