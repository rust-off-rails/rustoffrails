# Installing Rust off Rails

```sh
curl -fsSL https://rustoffrails.org/install.sh | sh
```

Read before you pipe: [`install.sh`](install.sh) is about 300 lines of POSIX shell. Run it with `--dry-run` to print the plan and every command without changing anything. Here's exactly what it does, in order.

## What the installer does

1. **Prints the plan** for your OS and architecture. Nothing has happened yet.
2. **Rust toolchain**:
   - If `cargo` exists, it moves on.
   - If not, it asks first, then runs the official rustup installer (`curl https://sh.rustup.rs | sh -s -- -y`). It **never uses sudo**.
3. **Cargo tools** used by the framework: `sea-orm-cli` (migrations), `cargo-nextest` (tests), `cargo-watch` (`bin/dev`). Only the missing ones are installed.
   - Preferred route: install [cargo-binstall](https://github.com/cargo-bins/cargo-binstall) with its official script, then `cargo binstall` prebuilt binaries (seconds).
   - If that fails, it falls back to `cargo install --locked` (minutes) and warns you.
4. **The framework**:
   - Downloads `rustoffrails.tar.gz` and `checksums.txt` from the GitHub release: latest, or `--version X.Y.Z`.
   - **Verifies the SHA-256** and refuses to install on a mismatch. Downloads also require `checksums.txt.sig`, an Ed25519 signature of `checksums.txt` verified against the release public key embedded in `install.sh` (the private key is held offline by the maintainer; `OFFRAILS_PUBKEY_FILE` overrides the key for rotation or tests). A local `OFFRAILS_SOURCE` without a signature installs with a warning, since a directory you pointed at is already trusted.
   - Unpacks to `~/.offrails/<version>/` and points `~/.offrails/current` at it.
   - Re-running with the same release is a no-op.
5. **The `offrails` command**: copies `~/.offrails/current/tools/offrails` to `~/.local/bin/offrails`. If that directory isn't on your `PATH`, it tells you the exact line to add.
6. **Coding agents**:
   - If `claude` (Claude Code) is on your PATH, it adds the marketplace from the checksum-verified framework copy it just installed (`~/.offrails/current`) and installs the plugin: `claude plugin marketplace add ~/.offrails/current`, then `claude plugin install offrails@offrails --scope user`. It skips this if the plugin is already installed.
   - For Codex, Cursor or any other agent, it prints the one-line prompt that points the agent at `~/.offrails/current/AGENTS.md`.
7. **Optional tools**: it only *detects* Docker (local Postgres, image builds, deploys), Node 24 and pnpm (only for `inertia`/`spa`/islands apps). It doesn't install them.

It sends no telemetry. Output is coloured only on a terminal (set `NO_COLOR=1` to disable). Any failure exits non-zero with a clear message.

## Options

| Flag / variable | Effect |
|---|---|
| `--dry-run` | Print the plan and commands; change nothing |
| `-y`, `--yes` | Don't ask (CI, scripts) |
| `--version X.Y.Z` | Install a specific release |
| `--no-toolchain` | Skip Rust and the cargo tools |
| `--no-agents` | Skip the Claude Code plugin |
| `OFFRAILS_HOME` | Framework location (default `~/.offrails`) |
| `OFFRAILS_BIN_DIR` | Where `offrails` goes (default `~/.local/bin`) |
| `OFFRAILS_SOURCE` | A directory or URL holding `rustoffrails.tar.gz` + `checksums.txt` (offline installs, mirrors; URL sources also need `checksums.txt.sig`) |

Pass options through the pipe like this: `curl -fsSL https://rustoffrails.org/install.sh | sh -s -- --dry-run`.

## The `offrails` command

```sh
offrails new bookshelf "people track the books they've read"   # Claude Code: /offrails:new …; else Codex; else prints the prompt
offrails doctor                                                # table of prerequisites and versions
offrails version
offrails update [--version X.Y.Z]                              # re-runs the installer
offrails uninstall                                             # removes ~/.offrails and the command (not Rust, not your apps)
offrails <command> --help                                      # that command's help; never runs anything
```

`new` and `upgrade` start an agent (Claude Code, else opencode, else Codex; `OFFRAILS_AGENT` picks one). When stdin is not a terminal (CI, a script, another agent) they need `--yes` or `OFFRAILS_YES=1`; without it they print the agent command and exit 3. Exit codes: 0 ok, 1 error, 2 usage (unknown command or flag), 3 agent launch needs `--yes`, 10 `upgrade --check` found an upgrade due.

## Manual install (same result, by hand)

```sh
# 1. Rust + tools
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo install --locked sea-orm-cli cargo-nextest cargo-watch

# 2. Framework (verify the checksum yourself)
V=0.3.0   # see https://github.com/rust-off-rails/rustoffrails/releases
curl -fsSLO https://github.com/rust-off-rails/rustoffrails/releases/download/v$V/rustoffrails.tar.gz
curl -fsSLO https://github.com/rust-off-rails/rustoffrails/releases/download/v$V/checksums.txt
shasum -a 256 -c checksums.txt --ignore-missing
mkdir -p ~/.offrails && tar -xmzf rustoffrails.tar.gz -C ~/.offrails && mv ~/.offrails/rustoffrails ~/.offrails/$V && ln -sfn $V ~/.offrails/current

# 3. Claude Code plugin (or skip and point any agent at ~/.offrails/current/AGENTS.md)
claude plugin marketplace add rust-off-rails/rustoffrails
claude plugin install offrails@offrails
```

You can also skip the installer entirely: clone the repo and tell your agent `Read /path/to/rustoffrails/AGENTS.md and make me a new app called bookshelf`.

## Uninstall

`offrails uninstall`, then optionally `claude plugin uninstall offrails@offrails && claude plugin marketplace remove offrails`. Rust and the cargo tools stay; remove them with `rustup self uninstall` if you want.
