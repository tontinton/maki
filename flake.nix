{
  description = "Maki - AI coding agent";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      lib = nixpkgs.lib;
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      packageName = cargoToml.package.name;
      version = cargoToml.workspace.package.version;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forEachSystem = f: lib.genAttrs systems (system: f system (import nixpkgs { inherit system; }));
    in
    {
      packages = forEachSystem (
        system: pkgs:
        let
          maki = pkgs.rustPlatform.buildRustPackage {
            pname = packageName;
            inherit version;
            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              # NOTE: these are cargo git dependencies; set hash to "" and
              # rebuild to get the correct value.
              outputHashes = {
                "monty-0.0.9" = "sha256-lIuPWXuovY4TB5M7JUCDAIN97bo1X8B6MhL3UjFTnqA=";
                "ruff_python_ast-0.0.0" = "sha256-nVQC4ZaLWiZBUEReLqzpXKxXVxCdUW6b+mda9J8JSA0=";
                "ruff_python_parser-0.0.0" = "sha256-nVQC4ZaLWiZBUEReLqzpXKxXVxCdUW6b+mda9J8JSA0=";
                "ruff_python_trivia-0.0.0" = "sha256-nVQC4ZaLWiZBUEReLqzpXKxXVxCdUW6b+mda9J8JSA0=";
                "ruff_source_file-0.0.0" = "sha256-nVQC4ZaLWiZBUEReLqzpXKxXVxCdUW6b+mda9J8JSA0=";
                "ruff_text_size-0.0.0" = "sha256-nVQC4ZaLWiZBUEReLqzpXKxXVxCdUW6b+mda9J8JSA0=";
              };
            };
            cargoBuildFlags = [
              "--package"
              packageName
            ];
            nativeBuildInputs = with pkgs; [
              pkg-config
              perl
              python3
            ];
            # TODO: Upstream monty includes a relative README path that doesn't
            # survive nix vendoring. Remove this once `monty` stops including
            # the relative path
            postPatch = ''
              for f in "$cargoDepsCopy"/monty-*/src/lib.rs; do
                substituteInPlace "$f" \
                  --replace-fail '#![doc = include_str!("../../../README.md")]' \
                                 '#![doc = "Monty Python bridge."]'
              done
            '';
            buildInputs = with pkgs; [ openssl ];
            doCheck = false;
          };
        in
        {
          default = maki;
        }
      );

      devShells = forEachSystem (
        _: pkgs:
        let
          certs = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              cargo-nextest
              clippy
              git
              just
              openssl
              perl
              pkg-config
              python3
              ripgrep
              ruff
              rustc
              rustfmt
              ty
            ];

            SSL_CERT_FILE = certs;
            NIX_SSL_CERT_FILE = certs;
          };
        }
      );

      formatter = forEachSystem (_: pkgs: pkgs.nixfmt-rfc-style);
    };
}
