# Emacs with the native Wayland (pwayl) + Skia rendering backend.
#
# Built from the ezemtsov/emacs `wayland-31` branch: Emacs 31 plus the pwayl
# window system and Skia renderer, on top of nixpkgs' emacs31 build.
{
  lib,
  emacs31,
  fetchgit,
  autoconf,
  automake,
  wayland-scanner,
  fontconfig,
  freetype,
  giflib,
  libjpeg,
  libpng,
  libtiff,
  librsvg,
  skia,
  libepoxy,
  wayland,
  wayland-protocols,
  libxkbcommon,
}:

(emacs31.override {
  withX = false;
  withCairo = false;
  withGTK3 = false;
  withPgtk = false;
  withXwidgets = false;
  withToolkitScrollBars = false;
}).overrideAttrs (old: {
  pname = "emacs31-pwayl";
  version = "31.1";

  src = fetchgit {
    url = "https://codeberg.org/ezemtsov/emacs.git";
    rev = "05e0d94d35e5c90a7f70c9435d50f9f0eba90bcf"; # wayland-31.1
    hash = "sha256-jAWiUYpi37IDTk226R+lqF2m6B8ext1LwyTE7qzhR5o=";
  };

  # Drop the image opt-outs the bare (no-GTK) build inherits, then turn the
  # backends and image support we want back on.
  configureFlags =
    lib.filter
      (flag: !(lib.elem flag [ "--without-gif" "--without-jpeg" "--without-png" "--without-tiff" ]))
      (old.configureFlags or [])
    ++ [
      "--with-rsvg"
      "--with-pwayl"
      "--with-skia"
      "--with-gif"
      "--with-jpeg"
      "--with-png"
      "--with-tiff"
    ];

  buildInputs = old.buildInputs ++ [
    fontconfig
    freetype
    giflib
    libjpeg
    libpng
    libtiff
    librsvg
    skia
    libepoxy
    wayland
    wayland-protocols
    libxkbcommon
  ];

  nativeBuildInputs = old.nativeBuildInputs ++ [ autoconf automake wayland-scanner ];

  preConfigure = (old.preConfigure or "") + ''
    ./autogen.sh
  '';
})
