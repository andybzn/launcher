{ pkgs, lib, ... }:

let
  deps = import ./deps.nix { inherit pkgs; };
in
{
  languages.rust = {
    enable = true;
    # The official toolchain: nixpkgs rustc's profiler runtime does not match its LLVM, so
    # coverage builds crash at exit. llvm-tools ships llvm-cov and llvm-profdata for cargo-llvm-cov.
    channel = "nightly";
    components = [ "rustc" "cargo" "clippy" "rustfmt" "llvm-tools-preview" ];
    lsp.enable = false;
  };

  packages =
    with pkgs;[
      ripgrep
      fd
      bacon
      # Its own tests run on nixpkgs rustc in the Nix build and crash there (see channel above).
      (cargo-llvm-cov.overrideAttrs { doCheck = false; })
    ]
    ++ lib.unique (deps.nativeBuildInputs ++ deps.buildInputs ++ deps.runtimeLibs);

  env.LD_LIBRARY_PATH = lib.makeLibraryPath deps.runtimeLibs;

  enterTest = ''
    set -uo pipefail
    status=0
    cargo fmt --all -- --check || status=1
    cargo clippy --all-targets --locked || status=1
    # Runs the tests with coverage. The baseline is 99.8% of lines and 99.7% of regions: four
    # lookups of an index from matches() or wrap() cannot fail, so their exits never run. The
    # settings take every key, so launch() never sees Enter there.
    # main() opens a window, so no test runs it; #[coverage(off)] leaves it out on nightly.
    cargo llvm-cov --locked --fail-under-lines 99.5 --fail-under-regions 99.5 || status=1
    exit "$status"
  '';
}
