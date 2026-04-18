{
 description = "clipboard manager with compression and blob storage";

 inputs = {
   nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
   flake-utils.url = "github:numtide/flake-utils";
 };

 outputs = { self, nixpkgs, flake-utils, ... }:
   flake-utils.lib.eachDefaultSystem (system:
     let
       pkgs = import nixpkgs { inherit system; };
     in
     with pkgs;
     {
       packages.default = rustPlatform.buildRustPackage rec {
         pname = "nocb";
         version = "0.2.0";
         
         src = ./.;
         
         cargoLock = {
           lockFile = ./Cargo.lock;
         };
         
         nativeBuildInputs = [ pkg-config ];
         buildInputs = [ sqlite libX11 libxcb wayland wayland-protocols ];
         
         meta = {
           description = "clipboard manager with compression and blob storage";
           homepage = "https://github.com/hitchhooker/nocb";
           license = lib.licenses.mit;
           mainProgram = "nocb";
         };
       };

       devShells.default = mkShell {
         buildInputs = [
           rustc
           cargo
           pkg-config
           sqlite
           libX11
           libxcb
           wayland
           wayland-protocols
         ];
       };
     });
}
