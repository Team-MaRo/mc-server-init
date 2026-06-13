# AGENTS.md

Guidance for working in this repository. Single source of truth — `CLAUDE.md`
just points here.

## What this is

`mc-server-init` is a tiny **PID-1 init for Minecraft server containers** (Spigot,
Paper, …), written in Rust. It replaces itzg's `mc-server-runner`. The one thing
it adds: it runs the server behind a **pseudo-terminal (PTY)** instead of a plain
pipe, so the server's JLine console detects a real terminal and keeps the `>`
prompt + line-editing. On top of that it:

- forwards the container's own stdin (interactive `docker run -it` / `docker
  attach`) into the server console;
- forwards a **named pipe** (FIFO, default `/tmp/console-in`) into the console so
  commands can be injected without RCON (`echo "say hi" > /tmp/console-in`);
- turns `SIGTERM` / `SIGINT` / a typed **Ctrl+C** (raw `0x03` byte) into a clean
  `stop`, so the world saves (with the "Saving…" logs), `SIGKILL` only after
  `--stop-timeout`;
- reaps the child as PID 1 and propagates its exit code.

It is consumed by [docker-spigot](https://github.com/D3strukt0r/docker-spigot) as
a Nix flake input (the image bakes the binary in and `exec`s it as PID 1).

## Architecture (all in `src/main.rs`)

One supervisor process pumps bytes between four file descriptors and translates
signals into a `stop`.

- **`Cli`** (clap derive) — the CLI: `--console-pipe`, `--stop-timeout`,
  `--stop-command`, and `argv` (everything after `--`, the server command). clap
  gives `--help`/`--version`/validation for free.
- **`run`** — sets up a PTY (`openpty`), blocks the managed signals, then
  `fork`s. The **child** becomes a session leader on the PTY slave
  (`setsid` + `TIOCSCTTY` + `dup2` onto 0/1/2), unblocks signals, and `execvp`s
  the server. The **parent** closes its slave copy (so the master sees EOF when
  the server dies), creates a **signalfd**, opens the console FIFO `O_RDWR` (so it
  never EOFs), and puts stdin in raw mode (to see Ctrl+C as `0x03`).
- **`event_loop`** — `poll`s the PTY master, the FIFO, the signalfd, and stdin;
  routes server↔terminal bytes, injects FIFO commands, intercepts Ctrl+C, and on
  SIGTERM/SIGINT/Ctrl+C sends `stop` with a SIGKILL deadline. SIGWINCH resizes the
  PTY. Exits with the reaped child's code (`128+signal` if it was killed).
- Helpers wrap the libc `read`/`write`/`ioctl`/`waitpid`/`termios` calls.

Signals are **blocked before fork** and read via the signalfd inside `poll`, so
they're handled synchronously (no async-handler races, no `unsafe` handler
restrictions).

## Layout

| Path | Purpose |
| --- | --- |
| `src/main.rs` | The entire program (CLI + supervisor + event loop + helpers). |
| `Cargo.toml` / `Cargo.lock` | Deps: `nix` (syscalls; re-exports `libc`) + `clap` (CLI). Lock is committed (binary crate + the flake's `cargoLock.lockFile` needs it). |
| `flake.nix` | `packages.<sys>.default` (glibc, via `rustPlatform.buildRustPackage`) and `packages.<sys>.mc-server-init-static` (fully static musl, via `pkgsStatic`). Also `overlays.default`. `version` carries the `# x-release-please-version` marker. |
| `.github/workflows/build.yml` | Per-arch native build (x86_64 + aarch64); on a release tag, patchelf's the glibc binary for portability and attaches both glibc + musl binaries (provenance-attested). |
| `.github/workflows/release.yml` | release-please (`rust`) → release PR → tag → GitHub Release. |
| `.devcontainer/` | Rust devcontainer (the crate can't build on a Windows host — see gotchas). |

## Build & dev

```bash
# Nix (preferred): glibc-dynamic and fully-static musl
nix build .#packages.x86_64-linux.default              # -> result/bin/mc-server-init (glibc)
nix build .#packages.x86_64-linux.mc-server-init-static # -> static musl (Alpine-friendly)

# Cargo (Linux host)
cargo build --release
```

No Nix on the host? Build inside the `nixos/nix` container (mirrors docker-spigot):

```bash
docker run --rm -v <nix-volume>:/nix -v "$PWD:/work" -w /work nixos/nix \
  sh -c "nix --extra-experimental-features 'nix-command flakes' build .#packages.x86_64-linux.default"
```

**Cargo edits need the working tree visible to Nix:** flake builds only see
git-tracked files. `cargo build` (reading the dir directly) doesn't, so it's the
quick way to recompile after edits.

## Targets & artifacts

**Linux x86_64 + aarch64 only.** There is no macOS/Windows build — the code uses
`fork`, `openpty`, `signalfd`, `setsid`, PID-1 reaping, which are Linux-container
constructs; it won't even compile on Windows (`nix` crate is Unix-only). Per arch,
releases attach:

- `mc-server-init-linux-<arch>` — glibc-dynamic. A nix binary hardcodes the
  `/nix/store` ELF interpreter, so CI `patchelf`s it to the system loader +
  removes the rpath; it then needs only `GLIBC_2.34` (any current distro).
- `mc-server-init-linux-<arch>-musl` — fully static (musl, `static-pie`); no libc
  dependency, runs on **Alpine**/distroless/any Linux as-is.

## Releases

Conventional commits → release-please (`release-type: rust`, configured in
`release-please-config.json` + `.release-please-manifest.json`) opens a release PR
bumping `Cargo.toml`/`Cargo.lock` and `flake.nix` (the `x-release-please-version`
marker). Merging cuts a tag + GitHub Release; the tag fires `build.yml` to attach
the binaries. Needs the `GH_PAT` secret and "Allow Actions to create PRs".

## Conventions & gotchas

- **Commits:** Conventional Commits (drives release-please + the changelog).
- **Line endings:** `.gitattributes` forces LF on `*.rs`/`*.nix`/`*.toml`/lock/
  yaml/etc. — Nix reads inline `''…''` build scripts literally, so CRLF breaks
  builds with `$'\r': command not found`.
- **Windows dev:** the crate can't compile on a Windows host (Linux-only syscalls
  + MSVC linker). Use the `.devcontainer/` (Reopen in Container) or WSL.
- **clap requires `--`:** the server command must follow `--`
  (`mc-server-init … -- java …`); `argv` uses `#[arg(last = true)]`.
- **Dependabot targets `develop`** (`.github/dependabot.yml`); until that branch
  exists, Dependabot will report a config error for those blocks — expected.
- Bumping the `nix` crate across majors: prefer keeping the `openpty` + `fork`
  approach (not `forkpty`, whose return type churned across 0.30/0.31).
