{
  description = "mc-server-init — tiny PID-1 init for Minecraft server containers (PTY console, named-pipe injection, graceful stop)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  };

  outputs = { self, nixpkgs }:
    let
      # Linux only: the init is built on fork + openpty + signalfd + PID-1
      # reaping — a Linux-container construct. There is no macOS/Windows build.
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems
        (system: f (import nixpkgs { inherit system; }));

      version = "0.1.2"; # x-release-please-version
    in
    {
      packages = forAllSystems (pkgs:
        let
          # Build the crate against a given package set. Used twice: once with
          # the normal (glibc) pkgs, once with pkgs.pkgsStatic (musl + fully
          # static) for an Alpine/anywhere binary.
          mkInit = p: p.rustPlatform.buildRustPackage {
            pname = "mc-server-init";
            inherit version;
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            meta = {
              description = "Tiny PID-1 init for Minecraft server containers";
              mainProgram = "mc-server-init";
              platforms = nixpkgs.lib.platforms.linux;
              license = nixpkgs.lib.licenses.mit;
            };
          };
          mc-server-init = mkInit pkgs;
        in
        {
          inherit mc-server-init;
          default = mc-server-init;
          # Fully static musl build: no dynamic libc, no ELF interpreter — runs
          # on Alpine, distroless, and any other Linux as-is (no patchelf).
          mc-server-init-static = mkInit pkgs.pkgsStatic;
        });

      # Overlay so a downstream flake can use `pkgs.mc-server-init` after
      # adding this flake's overlay (an alternative to referencing the package
      # output directly, which is what docker-spigot does).
      overlays.default = final: _prev: {
        mc-server-init = self.packages.${final.stdenv.hostPlatform.system}.default;
      };
    };
}
