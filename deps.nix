# Native libraries shared by the Nix package and the devenv shell.
{ pkgs }:
with pkgs;
{
  nativeBuildInputs = [
    makeWrapper
    pkg-config
  ];
  buildInputs = [
    fontconfig
    libxkbcommon
  ];
  # winit and wgpu dlopen these at runtime.
  runtimeLibs = [
    vulkan-loader
    wayland
    libxkbcommon
    fontconfig
    libx11
    libxcursor
    libxi
    libxrandr
  ];
}
