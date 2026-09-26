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
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.installShellFiles ];
          postInstall = ''
            $out/bin/jotter --generate gen
            installManPage gen/man/jotter.1
            installShellCompletion gen/completions/{jotter.bash,jotter.fish,_jotter}
          '';
          # tests spawn no kernels (the real-kernel test is #[ignore]d)
          meta = {
            description = "A fast Jupyter notebook TUI";
            mainProgram = "jotter";
            license = with nixpkgs.lib.licenses; [ mit asl20 ];
          };
        };
        default = jotter;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ rustc cargo clippy rustfmt rust-analyzer ];
        };
      } // nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
        # `nix develop .#docs`: the docs site and its media recording rig
        # (docs/media/record.nu). Everything a recording depends on is pinned
        # here, so clips come out the same locally and in CI.
        docs = pkgs.mkShell {
          packages = with pkgs; [
            rustc cargo nodejs_22 nushell
            sway wf-recorder grim ffmpeg kitty
            (python3.withPackages (p: with p; [ ipykernel numpy matplotlib tqdm sympy ]))
          ];
          # software OpenGL from this Mesa for kitty, also where the host has
          # no GPU drivers (CI runners)
          __EGL_VENDOR_LIBRARY_FILENAMES = "${pkgs.mesa}/share/glvnd/egl_vendor.d/50_mesa.json";
          LIBGL_DRIVERS_PATH = "${pkgs.mesa}/lib/dri";
          GBM_BACKENDS_PATH = "${pkgs.mesa}/lib/gbm";
          LIBGL_ALWAYS_SOFTWARE = "1";
          FONTCONFIG_FILE = pkgs.makeFontsConf {
            fontDirectories = with pkgs; [ jetbrains-mono dejavu_fonts noto-fonts-color-emoji ];
          };
        };
      });
    };
}
