{
  description = "apisip: point it at an OpenAPI spec, get JSONL or Parquet files";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "apisip";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # Tests that need the network or an S3 server are #[ignore]d or gated,
          # so the sandbox runs the rest. The HTTP client loads CA certificates
          # even for plain-HTTP test servers.
          doCheck = true;
          # Debug tests: the crash-safety tests use fail points that release builds compile out.
          checkType = "debug";
          nativeCheckInputs = [
            pkgs.cacert
            pkgs.duckdb
          ];
          # The view checks run DuckDB; fail rather than skip if it is missing.
          APISIP_REQUIRE_DUCKDB = "1";
          meta = {
            description = "Ingest any API described by an OpenAPI spec into JSONL or Parquet";
            license = with pkgs.lib.licenses; [
              mit
              asl20
            ];
            mainProgram = "apisip";
          };
        };
      });

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.default}/bin/apisip";
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            duckdb
          ];
        };
      });

      checks = forAllSystems (pkgs: {
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
