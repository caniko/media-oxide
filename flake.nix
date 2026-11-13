{
  description = "Immich transpiler framework — transcode originals to AV1/JXL";

  inputs = {
    rs-harbor.url = "git+https://github.com/caniko/harbor-rs.git?ref=trunk&rev=05cc4f162b55fa904b687db1821e2463fa813e50";
    nixpkgs.follows = "rs-harbor/nixpkgs";
    rust-overlay.follows = "rs-harbor/rust-overlay";
    crane.follows = "rs-harbor/crane";
    flake-parts.url = "github:hercules-ci/flake-parts";
  };

  outputs = {
    self,
    nixpkgs,
    rs-harbor,
    flake-parts,
    rust-overlay,
    ...
  } @ inputs:
    flake-parts.lib.mkFlake {inherit inputs;} {
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];

      flake = let
        nixosModule = import ./nixos-modules/immich-convert-originals.nix;
      in {
        nixosModules.immich-convert-originals = nixosModule;
        nixosModules.default = nixosModule;
      };

      perSystem = {system, ...}: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        };

        toolchain = rs-harbor.lib.mkToolchain {
          inherit pkgs;
          channel = "stable";
          extensions = ["rust-src" "rustfmt" "clippy" "llvm-tools-preview"];
          withRustAnalyzer = false;
          crossTargets = ["x86_64-unknown-linux-gnu"];
        };
        inherit (toolchain) craneLib;
        cross = rs-harbor.lib.mkCross {
          inherit pkgs system;
        };
        cargoConfig = rs-harbor.lib.mkCargoConfig {
          inherit pkgs;
          channel = "stable";
          enableCranelift = false;
          enableShareGenerics = false;
          enableParallelFrontend = false;
        };

        src = craneLib.cleanCargoSource ./.;
        cargoVendorDir = craneLib.vendorCargoDeps {
          inherit src;
        };

        nativeBuildInputs = with pkgs; [
          pkg-config
          clang
          mold
          # openssl-sys may invoke Perl while probing/building OpenSSL in the
          # sandbox, even when pkg-config supplies the library location.
          perl
        ];
        buildInputs = with pkgs; [
          openssl
        ];

        # Encoders are driven as CLIs: libjxl's cjxl and the ffmpeg build Immich
        # itself uses, which carries SVT-AV1 and libvmaf.
        mediaTools = with pkgs; [
          libjxl
          jellyfin-ffmpeg
        ];

        # Keep the derivation's pkg-config environment explicit. The fallback
        # lets this checkout remain usable with older locked rs-harbor inputs;
        # newer rs-harbor versions provide the same contract as a shared
        # helper for downstream consumers.
        pkgConfigEnv =
          if rs-harbor.lib ? mkPkgConfigEnv
          then
            rs-harbor.lib.mkPkgConfigEnv {
              inherit pkgs;
              deps = buildInputs;
            }
          else {
            PKG_CONFIG_PATH = pkgs.lib.makeSearchPathOutput "dev" "lib/pkgconfig" buildInputs;
          };

        commonArgs =
          {
            inherit src cargoVendorDir;
            strictDeps = true;
            inherit nativeBuildInputs buildInputs;
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          }
          // pkgConfigEnv;

        package = craneLib.buildPackage (
          commonArgs
          // {
            pname = "rs-immicher-oxide";
            nativeBuildInputs = nativeBuildInputs ++ [pkgs.makeWrapper];
            nativeCheckInputs = mediaTools;
            postInstall = ''
              wrapProgram $out/bin/rs-immicher-oxide \
                --prefix PATH : ${pkgs.lib.makeBinPath mediaTools}
            '';
          }
        );
      in {
        packages.default = package;

        checks = {
          clippy = craneLib.mkCargoDerivation (
            commonArgs
            // {
              cargoArtifacts = null;
              pnameSuffix = "-clippy";
              buildPhaseCargoCommand = "cargoWithProfile clippy --locked --all-targets -- --deny warnings";
            }
          );
          fmt = craneLib.cargoFmt {
            inherit src;
          };
        };

        devShells =
          (rs-harbor.lib.mkDevShells {
            inherit pkgs cross cargoConfig;
            inherit (toolchain) craneLib;
            pkgConfigDeps = buildInputs;
            packages = nativeBuildInputs ++ buildInputs ++ mediaTools;
          })
          // {
            docs = rs-harbor.lib.mkDocsShell {
              inherit pkgs cross cargoConfig;
              inherit (toolchain) craneLib;
              pkgConfigDeps = buildInputs;
              packages = [pkgs.mdbook] ++ nativeBuildInputs ++ buildInputs;
            };
          };
      };
    };
}
