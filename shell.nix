{ pkgs ? import <nixpkgs> {} }:
let
  rust-overlay = import (builtins.fetchTarball {
    url = "https://github.com/oxalica/rust-overlay/archive/master.tar.gz";
  });
  pkgs-with-overlay = import <nixpkgs> {
    overlays = [ rust-overlay ];
  };
  rust-nightly = pkgs-with-overlay.rust-bin.nightly.latest.default;
in
pkgs.mkShell {
  buildInputs = with pkgs; [
    # Build tools - nightly Rust
    rust-nightly
    pkg-config
    
    # X11/XCB dependencies
    xorg.libxcb
    xorg.libX11
    xorg.libXcursor
    xorg.libXrandr
    xorg.libXi

    # Wayland dependencies
    wayland
    wayland-protocols
    
    # OpenGL/Vulkan support
    libGL
    vulkan-loader
    vulkan-headers
    vulkan-validation-layers
    
    # libxkbcommon
    libxkbcommon
    
    # Mesa
    mesa
    
    # Optional: for development
    rust-analyzer
    clippy
    rustfmt
  ];
  
  # Set up pkg-config paths
  PKG_CONFIG_PATH = "${pkgs.xorg.libxcb}/lib/pkgconfig";
  
  # Set LD_LIBRARY_PATH for runtime
  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
    pkgs.libGL
    pkgs.xorg.libX11
    pkgs.xorg.libXcursor
    pkgs.xorg.libXrandr
    pkgs.xorg.libXi
    pkgs.vulkan-loader
    pkgs.libxkbcommon
  ];
}
