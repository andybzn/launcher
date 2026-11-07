{
  description = "Rune-based app launcher";

  inputs.nixpkgs.url = "https://channels.nixos.org/nixpkgs-unstable/nixexprs.tar.zst";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (s: f nixpkgs.legacyPackages.${s});
    in {
      packages = forAll (pkgs: rec {
        launcher = pkgs.rustPlatform.buildRustPackage {
          pname = "launcher";
          version = "0.1.0";
          src = self;
          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true;
          };
          nativeBuildInputs = [ pkgs.makeWrapper pkgs.pkg-config ];
          buildInputs = [ pkgs.fontconfig pkgs.libxkbcommon ];
          # winit and wgpu dlopen these at runtime.
          postFixup = ''
            wrapProgram $out/bin/launcher \
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath (with pkgs; [
                vulkan-loader wayland libxkbcommon fontconfig
                libx11 libxcursor libxi libxrandr
              ])}
          '';
        };
        default = launcher;
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.system}.default ];
          packages = [ pkgs.cargo pkgs.rustc ];
        };
      });
    };
}
