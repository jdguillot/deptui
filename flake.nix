{
  description = "deptui — a terminal UI and auto-deploy agent for serokell/deploy-rs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    deploy-rs.url = "github:serokell/deploy-rs";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      deploy-rs,
    }:
    (flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };

        # Single source of truth for the version: the workspace
        # manifest. The flake hardcoding its own copy is how releases
        # drift.
        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

        # Both binaries shell out at runtime; the wrapper is what makes
        # `nix run`/module installs work without polluting the user's
        # profile with nix/ssh/git pins.
        runtimeDeps = [
          deploy-rs.packages.${system}.deploy-rs
          pkgs.nix
          pkgs.openssh
        ];

        mkPackage =
          {
            pname,
            extraRuntime ? [ ],
          }:
          pkgs.rustPlatform.buildRustPackage {
            inherit pname version;
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            # One workspace, one lockfile, one package per binary.
            cargoBuildFlags = [
              "-p"
              pname
            ];
            cargoTestFlags = [
              "-p"
              pname
              "-p"
              "deptui-core"
            ];

            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.makeWrapper
              # The agent's tests drive real `git` repositories.
              pkgs.git
            ];
            buildInputs = [ pkgs.openssl ];

            postInstall = ''
              wrapProgram $out/bin/${pname} \
                --prefix PATH : ${pkgs.lib.makeBinPath (runtimeDeps ++ extraRuntime)}
            '';
          };
      in
      {
        devShells.default = pkgs.mkShell {
          name = "deptui-dev";

          packages = with pkgs; [
            # Rust toolchain
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer

            # Build tools
            pkg-config

            # Runtime tools the TUI and the agent shell out to
            deploy-rs.packages.${system}.deploy-rs
            nix
            openssh
            git
          ];

          # OpenSSL is unused at the moment but commonly needed once HTTPS
          # crates are added.
          buildInputs = with pkgs; [ openssl ];

          RUST_BACKTRACE = "1";
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };

        packages = {
          deptui = mkPackage { pname = "deptui"; };
          # `git` is the agent's own addition: ls-remote polling and the
          # private clones it deploys from.
          deptui-agent = mkPackage {
            pname = "deptui-agent";
            extraRuntime = [ pkgs.git ];
          };
          default = self.packages.${system}.deptui;
        };
      }
    ))
    // {
      nixosModules.deptui-agent = import ./nix/module.nix { inherit self; };
      nixosModules.default = self.nixosModules.deptui-agent;
    };
}
