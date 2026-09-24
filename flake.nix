{
  description = "jOtter — a fast Jupyter notebook TUI. The Jupyter otter. 🦦";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        jotter = pkgs.rustPlatform.buildRustPackage {
          pname = "jotter";
          version = "0.1.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          # tests spawn no kernels (the real-kernel test is #[ignore]d)
          meta = {
            description = "A fast Jupyter notebook TUI";
            mainProgram = "jotter";
          };
        };
        default = jotter;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ rustc cargo clippy rustfmt rust-analyzer ];
        };
      });
    };
}
