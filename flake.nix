# SPDX-License-Identifier: Apache-2.0

{
  description = "Development environment for zed-codespell-language-server";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [
          (import rust-overlay)
        ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        toolchain = pkgs.rust-bin.stable.latest.minimal.override {
          extensions = [
            "rustfmt"
            "clippy"
            "rust-src"
          ];
          # The extension host compiles to a WASI component (wasm32-wasip2);
          # wasip1 is kept for hosts still running older Zed builds.
          targets = [
            "wasm32-wasip1"
            "wasm32-wasip2"
          ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            toolchain
            pkgs.codespell
            pkgs.python3
          ];
          shellHook = ''
            echo "zed-codespell-language-server development environment"
            echo "Rust: $(rustc --version)"
            echo "codespell: $(codespell --version 2>/dev/null || echo not found)"
            echo ""
            echo "Commands:"
            echo "  make test           - Run cargo test (root + lsp crate)"
            echo "  make build          - Build the extension host as a WASM component"
            echo "  make build-native   - Build the codespell-lsp adapter binary"
            echo "  make dev            - Dev extension install instructions"
            echo "  make check          - fmt + clippy + test"
          '';
        };
      }
    );
}
