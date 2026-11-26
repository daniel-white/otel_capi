{
  description = "otel-capi - C API for OpenTelemetry";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-26.05-darwin";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachSystem [
      "x86_64-darwin"
      "aarch64-darwin"
      "x86_64-linux"
      "aarch64-linux"
    ] (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        # Rust 1.99+ is required to define C-variadic functions in Rust itself.
        rustToolchain = pkgs.rust-bin.stable."1.100.0".default;
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.pkg-config
            pkgs.gcc
            pkgs.clang
          ];

          env.RUST_BACKTRACE = "1";
        };
      });
}
