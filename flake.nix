{
  description = "Maki - AI coding agent";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      nixpkgs,
      crane,
      rust-overlay,
      ...
    }:
    let
      lib = nixpkgs.lib;
      cargoToml = fromTOML (builtins.readFile ./Cargo.toml);
      packageName = cargoToml.package.name;
      version = cargoToml.workspace.package.version;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forEachSystem =
        f:
        lib.genAttrs systems (
          system:
          f system (
            import nixpkgs {
              inherit system;
              overlays = [ (import rust-overlay) ];
            }
          )
        );

      mkCraneLib =
        pkgs: pickToolchain:
        (crane.mkLib pkgs).overrideToolchain (pickToolchain pkgs.rust-bin.stable."1.99.0");

      mkWorkspaceSrc =
        craneLib:
        lib.cleanSourceWith {
          filter =
            path: type:
            (craneLib.filterCargoSources path type)
            || (builtins.match ".*/plugins/.*" path != null)
            || (builtins.match ".*/prompts/.*" path != null)
            || (builtins.match ".*/themes/.*" path != null)
            || (builtins.match ".*/words/.*" path != null)
            || (lib.hasSuffix ".lua" path);
          src = lib.cleanSource ./.;
        };

      # The minimal toolchain skips rust-docs, clippy and rustfmt, which no build
      # needs. Dependencies are built without the `cargo check` and `cargo test
      # --no-run` passes crane runs by default: no build here runs tests.
      mkMaki =
        pkgs: profile:
        let
          craneLib = mkCraneLib pkgs (toolchains: toolchains.minimal);
          src = mkWorkspaceSrc craneLib;
          commonArgs = {
            pname = packageName;
            inherit src version;
            CARGO_PROFILE = profile;
            doCheck = false;
            nativeBuildInputs = with pkgs; [
              pkg-config
              python3
            ];
            buildInputs = with pkgs; [
              stdenv.cc.cc.lib
            ];
            cargoVendorDir = craneLib.vendorCargoDeps {
              inherit src;
              outputHashes = gitDepHashes;
            };
          };
        in
        craneLib.buildPackage (
          commonArgs
          // {
            cargoArtifacts = craneLib.buildDepsOnly (
              commonArgs // { buildPhaseCargoCommand = "cargoWithProfile build --locked"; }
            );
            cargoExtraArgs = "--package ${packageName}";
          }
        );

      cargoLockParsed = builtins.fromTOML (builtins.readFile ./Cargo.lock);

      # Exact Cargo.lock source strings (with fragment) of all git deps
      gitDepSources = lib.unique (
        builtins.filter (lib.hasPrefix "git+") (map (p: p.source or "") cargoLockParsed.package)
      );

      # Fixed-output fetches for git deps: cold evals skip full-history
      # builtins.fetchGit clones, and CI gets substitutable cache hits instead.
      # Keys embed the dep's tag/rev and locked commit, so a dep bump changes
      # the key itself: replace the old key with the new Cargo.lock source
      # string, set the hash to "", rebuild, and paste the hash from the error.
      # The `git-dep-hashes` CI check names both the key to add and the stale
      # key to remove.
      #
      # Two failure modes:
      # - Missing key (dep bumped, flake not yet updated): crane falls back
      #   to fetchGit with an eval warning. Nothing breaks; cold evals are
      #   just slower until the key and its hash are added.
      # - Wrong hash for an existing key: the build fails with a hash
      #   mismatch that prints the real hash. This is both the recovery
      #   path for updates and a hard stop if pinned content ever changes.
      gitDepHashes = {
        "git+https://github.com/crossterm-rs/crossterm?rev=94ed7154e35d24a2f643c61760e1eb05b8ca9e63#94ed7154e35d24a2f643c61760e1eb05b8ca9e63" =
          "sha256-r2Xsxw8+eqESsJKaax5pb/rIWshkoqi5E81Dkh8snZ8=";
        "git+https://github.com/tontinton/syntect?rev=01df275f6f25da670e5ba5b7128cb89d03795119#01df275f6f25da670e5ba5b7128cb89d03795119" =
          "sha256-ZszhXL+Bd0zZGOfOodvP4z6dLD3jJr3pO07riwFUC70=";
      };

      missingGitDepHashes = builtins.filter (s: !(builtins.hasAttr s gitDepHashes)) gitDepSources;

      staleGitDepHashes = builtins.filter (k: !(builtins.elem k gitDepSources)) (
        builtins.attrNames gitDepHashes
      );

      gitDepHashDrift =
        lib.optionalString (missingGitDepHashes != [ ]) ''
          missing entries (add with hash "", rebuild, paste the hash from the error):
            ${lib.concatStringsSep "\n  " missingGitDepHashes}
        ''
        + lib.optionalString (staleGitDepHashes != [ ]) ''
          stale entries (remove):
            ${lib.concatStringsSep "\n  " staleGitDepHashes}
        '';
    in
    {
      packages = forEachSystem (_: pkgs: { default = mkMaki pkgs "release"; });

      checks = forEachSystem (
        _: pkgs: {
          # Catches what only breaks under Nix (source filter, vendoring, native
          # inputs) in a fraction of the time of the fat LTO release build.
          build = mkMaki pkgs "dev";
          git-dep-hashes =
            if missingGitDepHashes == [ ] && staleGitDepHashes == [ ] then
              pkgs.runCommandLocal "git-dep-hashes" { } "touch $out"
            else
              builtins.throw ''
                flake.nix gitDepHashes is out of sync with Cargo.lock git sources:
                ${gitDepHashDrift}'';
          fmt =
            pkgs.runCommandLocal "check-nix-format"
              {
                nativeBuildInputs = [ pkgs.nixfmt ];
                src = lib.cleanSource ./.;
              }
              ''
                find "$src" -name '*.nix' -type f -exec nixfmt --check {} +
                touch $out
              '';
        }
      );

      devShells = forEachSystem (
        _: pkgs:
        let
          craneLib = mkCraneLib pkgs (
            toolchains:
            toolchains.default.override {
              extensions = [
                "rust-src"
                "rust-analyzer"
              ];
            }
          );
          certs = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        in
        {
          default = craneLib.devShell {
            packages = with pkgs; [
              cargo-machete
              cargo-nextest
              git
              just
              pkg-config
              python3
              ripgrep
              ruff
              stylua
              ty
            ];

            SSL_CERT_FILE = certs;
            NIX_SSL_CERT_FILE = certs;

            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
              pkgs.stdenv.cc.cc.lib
            ];
          };
        }
      );

      formatter = forEachSystem (_: pkgs: pkgs.nixfmt);
    };
}
