# facaded and the facade CLI, built from source with crane. The web UI is
# WebAssembly, built first with trunk and embedded into facaded (rust-embed reads
# crates/facade-ui/dist at compile time), as .github/workflows/build.yml does.
{
  pkgs,
  crane,
  # The commit, shown by `facaded --version` and in Settings → Updates.
  commit ? null,
}:
let
  inherit (pkgs) lib;

  toolchain =
    p: p.rust-bin.stable.latest.default.override { targets = [ "wasm32-unknown-unknown" ]; };
  craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      (lib.fileset.difference ../crates (lib.fileset.maybeMissing ../crates/facade-ui/dist))
    ];
  };

  common = {
    inherit src;
    version = (lib.importTOML ../Cargo.toml).workspace.package.version;
    strictDeps = true;
    # `cargo test` runs in CI's test job, and some tests need Docker.
    doCheck = false;
  };

  # Dependencies of the workspace's default members (daemon, CLI, core), built
  # once for both binaries.
  cargoArtifacts = craneLib.buildDepsOnly (common // { pname = "facade"; });

  # trunk needs the wasm-bindgen CLI of the version in Cargo.lock. When the lock
  # moves to another one: set it here, and both hashes to lib.fakeHash, then
  # `nix build .#facade-ui` says the right ones.
  wasmBindgenVersion = "0.2.129";
  locked = lib.findFirst (p: p.name == "wasm-bindgen") {
    version = null;
  } (lib.importTOML ../Cargo.lock).package;
  wasm-bindgen-cli =
    assert lib.assertMsg (locked.version == wasmBindgenVersion) ''
      Cargo.lock has wasm-bindgen ${toString locked.version}, nix/package.nix builds the
      CLI of ${wasmBindgenVersion}: update wasmBindgenVersion and its hashes there.
    '';
    pkgs.buildWasmBindgenCli rec {
      src = pkgs.fetchCrate {
        pname = "wasm-bindgen-cli";
        version = wasmBindgenVersion;
        hash = "sha256-pcecKQd7E8Opw6bkFoE569epUi7gh5qpQF1e5PJY6V8=";
      };
      cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
        inherit src;
        inherit (src) pname version;
        hash = "sha256-vmUrWVU7kPJJxO5qIVeAkwQyWDELO1Z4Z5gitz2kco8=";
      };
    };

  uiCommon = common // {
    pname = "facade-ui";
    cargoExtraArgs = "--locked --package facade-ui";
    CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
  };

  ui = craneLib.buildTrunkPackage (
    uiCommon
    // {
      cargoArtifacts = craneLib.buildDepsOnly uiCommon;
      inherit wasm-bindgen-cli;
      # From the crate's folder, where Trunk.toml is.
      buildPhaseCargoCommand = ''
        (cd crates/facade-ui && trunk build --release=true)
      '';
      installPhaseCommand = ''
        cp -r crates/facade-ui/dist $out
      '';
    }
  );

  bin =
    name: bin: extra:
    craneLib.buildPackage (
      common
      // {
        pname = name;
        inherit cargoArtifacts;
        cargoExtraArgs = "--locked --bin ${bin}";
        meta = {
          description = "facade, a self-hosted PaaS: ${name}";
          homepage = "https://github.com/Sigma-studios/facade";
          license = lib.licenses.mit;
          mainProgram = bin;
          platforms = lib.platforms.linux;
        };
      }
      // extra
    );
in
{
  inherit ui;

  facaded = bin "facaded" "facaded" (
    {
      preBuild = ''
        rm -rf crates/facade-ui/dist
        cp -r ${ui} crates/facade-ui/dist
        chmod -R u+w crates/facade-ui/dist
      '';
      passthru = { inherit ui; };
    }
    // lib.optionalAttrs (commit != null) { FACADE_COMMIT = commit; }
  );

  facade = bin "facade" "facade" { };
}
