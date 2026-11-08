{ pkgs, lib, ... }:

let
  deps = import ./deps.nix { inherit pkgs; };
in
{
  languages.rust = {
    enable = true;
    channel = "nixpkgs";
    components = [ "rustc" "cargo" "clippy" "rustfmt" ];
    lsp.enable = false;
  };

  packages =
    [
      pkgs.ripgrep
      pkgs.fd
    ]
    ++ lib.unique (deps.nativeBuildInputs ++ deps.buildInputs ++ deps.runtimeLibs);

  env.LD_LIBRARY_PATH = lib.makeLibraryPath deps.runtimeLibs;

  enterTest = ''
    set -uo pipefail
    status=0
    cargo fmt --all -- --check || status=1
    cargo clippy --all-targets --locked || status=1
    cargo test --locked || status=1
    exit "$status"
  '';
}
