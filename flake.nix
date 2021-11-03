{
  description = "A very basic flake";
  inputs = {
      nixpkgs.url = "github:nixos/nixpkgs/master";
      flake-utils.url = "github:numtide/flake-utils/master";
      naersk.url = "github:nmattia/naersk";
      rust-overlay.url = "github:oxalica/rust-overlay/master";
      rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
      rust-overlay.inputs.flake-utils.follows = "flake-utils";
  };
  outputs = { self, nixpkgs, rust-overlay, flake-utils, naersk }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ rust-overlay.overlay ];
        pkgs = import nixpkgs { inherit system overlays; };
        naersk-lib = naersk.lib.${system};
        my-rust = pkgs.rust-bin.nightly.latest.default.override {
          extensions = [ "rust-src" "llvm-tools-preview" ];
        };
      in {
        defaultPackage = naersk-lib.buildPackage rec {
          pname = "monitor-layout";
          root = ./.;
          nativeBuildInputs = with pkgs; [ scdoc installShellFiles ];
          preFixup = ''
            installManPage man/${pname}.*.scd
          '';
        };

        devShell = pkgs.mkShell {
          buildInputs = with pkgs; [
            linuxPackages.perf
            my-rust
            cargo-watch
            cargo-bloat
            cargo-binutils
            cargo-deps
            gdb-multitarget
            gnuplot
            scdoc
            xtruss # This is _really_ good; it's strace for xorg calls, with all that implies
            xdotool # has subcommands like "getactivewindow"  that are useful
            xorg.xwininfo
          ];
        };
      });
}
