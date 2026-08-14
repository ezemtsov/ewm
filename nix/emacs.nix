# Emacs with the native Wayland (pwayl) + Skia rendering backend.
#
# Built from the ezemtsov/emacs `wayland-31` branch: upstream master plus the
# pwayl window system and Skia renderer. Needs emacs-git from emacs-overlay,
# since the fork tracks master rather than the release nixpkgs ships.
{
  lib,
  emacs-git,
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

(emacs-git.override {
  withX = false;
  withCairo = false;
  withGTK3 = false;
  withPgtk = false;
  withXwidgets = false;
  withToolkitScrollBars = false;
}).overrideAttrs (old: {
  pname = "emacs31-pwayl";
  name = "emacs31-pwayl-${old.version}";

  src = fetchgit {
    url = "https://codeberg.org/ezemtsov/emacs.git";
    rev = "b04eab42f7fd933f1b8a4ebbaae9d38333af6ae7"; # wayland-31
    hash = "sha256-ma11Me2+9toAzU4GkgR2qQjaBYMUFT/MDWi/7Z83QFo=";
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
