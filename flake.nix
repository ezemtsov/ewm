{
  description = "EWM - Emacs Wayland Manager";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      perSystem =
        f:
        nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ] (
          system: f (nixpkgs.legacyPackages.${system}.extend self.overlays.default)
        );
    in
    {
      overlays.default = import ./nix/overlay.nix;

      nixosModules.default = import ./nix/service.nix;

      packages = perSystem (pkgs: {
        default = pkgs.ewm;
        inherit (pkgs) emacs31-pwayl;
        # Run outside the sandbox, see nix/vm-test.nix.
        session-test-pwayl = pkgs.callPackage ./nix/vm-test.nix { withSkia = true; };
        session-test-pgtk = pkgs.callPackage ./nix/vm-test.nix { withSkia = false; };
      });

      checks = perSystem (pkgs: {
        inherit (pkgs) ewm;
        ewm-pwayl = pkgs.ewm.override { emacsPackage = pkgs.emacs31-pwayl; };
      });

      devShells = perSystem (
        pkgs:
        let
          shell = packages: import ./shell.nix { inherit pkgs packages; };
        in
        {
          default = shell [ ];
          # rustfmt stays out of shell.nix, where the rustup nightly provides it.
          ci-pgtk = shell [
            pkgs.rustfmt
            pkgs.emacs-pgtk
          ];
          ci-pwayl = shell [
            pkgs.rustfmt
            pkgs.emacs31-pwayl
          ];
        }
      );
    };
}
