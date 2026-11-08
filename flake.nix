{
  description = "Rune-based app launcher";

  inputs.nixpkgs.url = "https://channels.nixos.org/nixpkgs-unstable/nixexprs.tar.zst";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (s: f nixpkgs.legacyPackages.${s});
    in {
      packages = forAll (pkgs:
        let deps = import ./deps.nix { inherit pkgs; };
        in rec {
          launcher = pkgs.rustPlatform.buildRustPackage {
            pname = "launcher";
            version = "0.1.0";
            src = self;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            nativeBuildInputs = deps.nativeBuildInputs;
            buildInputs = deps.buildInputs;
            # winit and wgpu dlopen these at runtime.
            postFixup = ''
              wrapProgram $out/bin/launcher \
                --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath deps.runtimeLibs}
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
