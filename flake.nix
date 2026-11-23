{
  description = "Token-lean semantic code search and project memory MCP server";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs }:
    let
      # Linux on x86-64 and ARM, and macOS on Apple silicon.
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      manifest = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package;
    in
    {
      packages = forAllSystems (pkgs: {
        context-broker = pkgs.rustPlatform.buildRustPackage {
          pname = manifest.name;
          inherit (manifest) version;
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ./tests ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          # The end-to-end test downloads the embedding model, and the build sandbox has no
          # network, so only the unit tests run here. CI runs the full suite.
          cargoTestFlags = [ "--bins" ];
          meta = {
            inherit (manifest) description;
            homepage = manifest.repository;
            mainProgram = "context-broker";
            platforms = systems;
          };
        };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.context-broker;
      });

      # `nixpkgs.overlays = [ context-broker.overlays.default ];` adds `pkgs.context-broker`.
      overlays.default = final: _prev: {
        context-broker = self.packages.${final.stdenv.hostPlatform.system}.context-broker;
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.context-broker ];
          packages = [ pkgs.clippy pkgs.rustfmt pkgs.rust-analyzer ];
        };
      });
    };
}
