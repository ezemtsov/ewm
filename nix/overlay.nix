# Package set EWM offers. Import after emacs-overlay, which provides emacs-git.
final: _prev: {
  ewm = final.callPackage ./default.nix { };
  emacs31-pwayl = final.callPackage ./emacs.nix { };
}
