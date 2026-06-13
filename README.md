# mc-server-init

A tiny PID-1 init for the Spigot container. It replaces itzg's
[`mc-server-runner`](https://github.com/itzg/mc-server-runner) with a
first-party binary, and adds the one thing that runner can't do: it runs the
server behind a **pseudo-terminal (PTY)**.

[![License](https://img.shields.io/github/license/Team-MaRo/mc-server-init)](LICENSE.txt)
[![Contributor Covenant](https://img.shields.io/badge/Contributor%20Covenant-2.0-4baaaa)][code-of-conduct]

[![GH Action Build](https://github.com/Team-MaRo/mc-server-init/actions/workflows/build.yml/badge.svg)][gh-action]

## Why a PTY

mc-server-runner pipes the server's stdin. Spigot's JLine console then sees that
stdin is *not* a terminal and drops to a dumb console — no `>` prompt, no
line-editing, no input echo. Running the server on a PTY makes JLine detect a
real terminal, so the interactive console (`>` prompt and all) comes back, while
we still control the master side for forwarding and graceful shutdown.

Trade-off: because the server writes to a terminal, `docker logs` may contain
terminal escape codes (colours, prompt redraws).

## What it does

- **PID 1 / init:** reaps the child and propagates its exit code.
- **PTY:** `openpty` + `fork`; the child becomes a session leader on the slave
  and `exec`s the server, so the console is fully interactive.
- **Input fan-in → server console:**
  - the container's own stdin (`docker run -it` / `docker attach`), and
  - a named pipe (FIFO, default `/tmp/console-in`) for scripted injection
    (`console <cmd>`, no RCON). Opened `O_RDWR` so it never EOFs.
- **Graceful stop:** on `SIGTERM`, `SIGINT`, or a typed Ctrl+C (the raw `0x03`
  byte off the terminal), it writes `stop\n` to the console so the world is
  saved (with the "Saving…" logs), then waits up to `--stop-timeout` seconds and
  sends `SIGKILL` only if the server overruns.
- **Window size:** propagates `SIGWINCH` (terminal resize) to the PTY.

## Usage

```sh
mc-server-init [--console-pipe PATH] [--stop-timeout SECS] [--stop-command CMD] -- <program> [args…]
```

Defaults: `--console-pipe /tmp/console-in`, `--stop-timeout 60`,
`--stop-command stop`. Example (how the container's entrypoint launches it):

```sh
mc-server-init --console-pipe /tmp/console-in --stop-timeout 60 -- java -jar /opt/spigot.jar --nogui
```

## Build & releases

**Linux only** — it uses `fork`/`openpty`/`signalfd`/`poll` + PID-1 reaping,
which are Linux-container constructs (no macOS/Windows build).

- **Nix flake:** `nix build .#packages.x86_64-linux.default` (or
  `aarch64-linux`) → `result/bin/mc-server-init`. A fully static **musl** build
  is also exposed: `nix build .#packages.x86_64-linux.mc-server-init-static`.
  Crate dependencies: [`nix`](https://crates.io/crates/nix) (syscalls; re-exports
  `libc`) and [`clap`](https://crates.io/crates/clap) (CLI parsing).
- **Cargo:** `cargo build --release` on a Linux host also works.
- **Releases:** [release-please](https://github.com/googleapis/release-please)
  cuts versioned GitHub Releases from conventional commits; CI attaches, per arch
  (amd64/arm64), build-provenance-attested binaries:
  - `mc-server-init-linux-<arch>` — glibc-dynamic (system ELF interpreter; needs
    glibc ≥ 2.34, i.e. any current distro).
  - `mc-server-init-linux-<arch>-musl` — **fully static** (musl); no libc
    dependency, runs on **Alpine**, distroless, and any other Linux as-is.

### Use from another Nix flake

```nix
{
  inputs.mc-server-init.url = "github:Team-MaRo/mc-server-init";
  # in outputs, for a given pkgs/system:
  #   mc-server-init.packages.${system}.default
  # or apply mc-server-init.overlays.default and use pkgs.mc-server-init
}
```

This is how the [docker-spigot](https://github.com/D3strukt0r/docker-spigot)
image consumes it (bakes the binary in and `exec`s it as PID 1).

## Contributing

Please read [CONTRIBUTING.md][contributing] for details on our code of conduct and the process for submitting pull requests.

This project uses [Conventional Commits](https://www.conventionalcommits.org/).

## Versioning

We use [SemVer](http://semver.org/) for versioning. For available versions, see the [tags on this repository][gh-tags].

## Authors

### Special thanks for all the people who had helped this project so far

- **Manuele** - [D3strukt0r](https://github.com/D3strukt0r)

See also the full list of [contributors][gh-contributors] who participated in this project.

### I would like to join this list. How can I help the project?

We're currently looking for contributions for the following:

- [ ] Bug fixes
- [ ] Translations
- [ ] etc...

For more information, please refer to our [CONTRIBUTING.md][contributing] guide.

## License

This project is licensed under the MIT License - see the [LICENSE.txt](LICENSE.txt) file for details.

## Acknowledgments

This project currently uses no third-party libraries or copied code.

[gh-action]: https://github.com/Team-MaRo/mc-server-init/actions
[gh-tags]: https://github.com/Team-MaRo/mc-server-init/tags
[gh-contributors]: https://github.com/Team-MaRo/mc-server-init/contributors
[contributing]: https://github.com/Team-MaRo/.github/blob/master/CONTRIBUTING.md
[code-of-conduct]: https://github.com/Team-MaRo/.github/blob/master/CODE_OF_CONDUCT.md
