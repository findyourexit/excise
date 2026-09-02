{
  description = "Excise, a surgical terminal storage navigator";

  inputs.nixpkgs.url = "https://api.flakehub.com/f/NixOS/nixpkgs/0.tar.gz";

  outputs = { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      packageFor = system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in pkgs.rustPlatform.buildRustPackage {
          pname = "excise";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = pkgs.lib.cleanSource self;
          cargoLock.lockFile = ./Cargo.lock;
          preCheck = ''
            cargo build --target ${pkgs.stdenv.hostPlatform.rust.rustcTargetSpec} --bin excise --locked --offline
            export EXCISE_PTY_DEBUG_BINARY="$PWD/target/${pkgs.stdenv.hostPlatform.rust.rustcTargetSpec}/debug/excise"
            cargo build --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTargetSpec} --bin excise --locked --offline
            export EXCISE_PTY_BINARY="$PWD/target/${pkgs.stdenv.hostPlatform.rust.rustcTargetSpec}/release/excise"
          '';
          nativeBuildInputs = [ pkgs.installShellFiles ];
          postInstall = ''
            installManPage generated/man/excise.1
            installShellCompletion \
              --bash generated/completions/excise.bash \
              --zsh generated/completions/_excise \
              --fish generated/completions/excise.fish
            install -Dm644 generated/completions/_excise.ps1 \
              $out/share/powershell/Modules/excise/_excise.ps1
            install -Dm644 generated/completions/excise.elv \
              $out/share/elvish/lib/excise.elv
            install -d $out/share/excise/schemas
            cp docs/schemas/*.json $out/share/excise/schemas/
          '';
          meta = {
            description = "Surgical terminal storage navigator";
            homepage = "https://github.com/findyourexit/excise";
            license = pkgs.lib.licenses.mit;
            mainProgram = "excise";
            platforms = pkgs.lib.platforms.unix;
          };
        };
    in {
      packages = forAllSystems (system: {
        default = packageFor system;
        excise = packageFor system;
      });
      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${packageFor system}/bin/excise";
        };
      });
      checks = forAllSystems (system: {
        package = packageFor system;
      });
      devShells = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = [ pkgs.cargo pkgs.rustc pkgs.rustfmt pkgs.clippy ];
          };
        });
    };
}
