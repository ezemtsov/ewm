# Package set EWM offers.
final: _prev: {
  ewm = final.callPackage ./default.nix { };
  emacs31-pwayl = final.callPackage ./emacs.nix { };
}
