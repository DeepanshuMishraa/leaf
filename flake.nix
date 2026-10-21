{
  description = "Leaf - Read PDFs, markdown files and web articles in the terminal.";

  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixpkgs-unstable/nixexprs.tar.zst";
  };

  outputs = inputs: {
    packages = builtins.mapAttrs (system: pkgs: {
      leaf = pkgs.callPackage ./package.nix { };

      default = inputs.self.packages.${system}.leaf;
    }) inputs.nixpkgs.legacyPackages;

    apps = builtins.mapAttrs (system: _: {
      default = {
        type = "app";
        program = "${inputs.self.packages.${system}.leaf}/bin/leaf";
      };
    }) inputs.nixpkgs.legacyPackages;

    devShells = builtins.mapAttrs (system: pkgs: {
      default = pkgs.mkShell {
        inputsFrom = [ inputs.self.packages.${system}.leaf ];
        packages = with pkgs; [
          rustc
          cargo
          rust-analyzer
          clippy
          rustfmt
        ];
      };
    }) inputs.nixpkgs.legacyPackages;

    formatter = builtins.mapAttrs (_: pkgs: pkgs.nixfmt) inputs.nixpkgs.legacyPackages;
  };
}
