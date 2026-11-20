# Emacs with the native Wayland (pwayl) + Skia rendering backend.
#
# Built from the ezemtsov/emacs `wayland-<release>` branch: Emacs plus the
# pwayl window system and Skia renderer, on top of nixpkgs' emacs31 build.
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
  release ? "31",
}:
let
  source = {
    "31" = {
      version = "31.1";
      rev = "c05c1af88802063fa6d02f80dd604141c157c6f7";
      hash = "sha256-bFe/Y1GWb87WHobfA1F9CaMBCi2L/b/xXXX8iQG2dD4=";
    };
    "32" = {
      version = "32.0.50";
      rev = "4722ef91783b673f544a7461e9864430f906656b";
      hash = "sha256-5Fb3RB27yUhACrxrgiH4LLy2Mpe7RFgQesS8ti3PR28=";
    };
  }.${release};
in
(emacs31.override {
  withX = false;
  withCairo = false;
  withGTK3 = false;
  withPgtk = false;
  withXwidgets = false;
  withToolkitScrollBars = false;
}).overrideAttrs (old: {
  pname = "emacs${release}-pwayl";
  inherit (source) version;

  src = fetchgit {
    url = "https://codeberg.org/ezemtsov/emacs.git";
    inherit (source) rev hash;
  };

  # Both branches already carry these fixes; nixpkgs' backports no longer apply.
  patches = lib.filter
    (p: !lib.any (name: lib.hasSuffix name (toString p))
       [ "CVE-2024-53920.patch" "CVE-2026-108976.patch" ])
    old.patches;

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
