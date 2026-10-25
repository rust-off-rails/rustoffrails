#!/bin/sh
# Rust off Rails installer.
#
#   curl -fsSL https://rustoffrails.org/install.sh | sh
#
# What it does (read INSTALL.md for the long version):
#   1. checks for Rust (offers rustup if missing) and installs the cargo tools the framework uses
#      (sea-orm-cli, cargo-nextest, cargo-watch) as prebuilt binaries via cargo-binstall
#   2. downloads the framework release, verifies its SHA-256, and installs it to ~/.offrails/<version>
#   3. installs the Claude Code plugin if `claude` is on your PATH
#   4. installs the `offrails` command to ~/.local/bin
#
# Never uses sudo. No telemetry. Safe to re-run.
#
# Options:
#   -y, --yes          answer yes to every question (non-interactive)
#   --dry-run          print the plan and exit without changing anything
#   --version X.Y.Z    install a specific release instead of the latest
#   --no-toolchain     skip Rust and the cargo tools (framework + plugin + command only)
#   --no-agents        skip the Claude Code plugin step
#   -h, --help         show this help
#
# Environment:
#   OFFRAILS_HOME      where the framework lives (default ~/.offrails)
#   OFFRAILS_BIN_DIR   where the `offrails` command goes (default ~/.local/bin)
#   OFFRAILS_SOURCE    a directory, file:// URL or https:// URL holding rustoffrails.tar.gz and
#                      checksums.txt (offline installs, mirrors, testing)

set -eu

REPO="rust-off-rails/rustoffrails"
OFFRAILS_HOME="${OFFRAILS_HOME:-$HOME/.offrails}"
BIN_DIR="${OFFRAILS_BIN_DIR:-$HOME/.local/bin}"
ASSET="rustoffrails.tar.gz"
SUMS="checksums.txt"

YES=0
DRY=0
WANT_VERSION=""
TOOLCHAIN=1
AGENTS=1

# ── output ────────────────────────────────────────────────────────────────────
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  B=$(printf '\033[1m'); D=$(printf '\033[2m'); R=$(printf '\033[31m'); G=$(printf '\033[32m')
  Y=$(printf '\033[33m'); E=$(printf '\033[38;5;208m'); N=$(printf '\033[0m')
else
  B=""; D=""; R=""; G=""; Y=""; E=""; N=""
fi
say()  { printf '%s\n' "$*"; }
step() { printf '%s==>%s %s%s%s\n' "$E" "$N" "$B" "$*" "$N"; }
ok()   { printf '  %s✓%s %s\n' "$G" "$N" "$*"; }
note() { printf '  %s·%s %s\n' "$D" "$N" "$*"; }
warn() { printf '  %s!%s %s\n' "$Y" "$N" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "$R" "$N" "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }
# shellcheck disable=SC2088 # a literal ~ for display only
pretty() { case "$1" in "$HOME"/*) printf '~/%s' "${1#"$HOME"/}" ;; *) printf '%s' "$1" ;; esac; }

usage() {
  cat <<'USAGE'
Rust off Rails installer: curl -fsSL https://rustoffrails.org/install.sh | sh

Options:
  -y, --yes          answer yes to every question (non-interactive)
  --dry-run          print the plan and exit without changing anything
  --version X.Y.Z    install a specific release instead of the latest
  --no-toolchain     skip Rust and the cargo tools (framework + plugin + command only)
  --no-agents        skip the Claude Code plugin step
  -h, --help         show this help

Environment: OFFRAILS_HOME (~/.offrails), OFFRAILS_BIN_DIR (~/.local/bin),
OFFRAILS_SOURCE (directory or URL with rustoffrails.tar.gz + checksums.txt).
See INSTALL.md for exactly what each step does.
USAGE
}

while [ $# -gt 0 ]; do
  case "$1" in
    -y|--yes) YES=1 ;;
    --dry-run) DRY=1 ;;
    --version) [ $# -ge 2 ] || die "--version needs a value, e.g. --version 0.3.0"; WANT_VERSION=${2#v}; shift ;;
    --version=*) WANT_VERSION=${1#--version=}; WANT_VERSION=${WANT_VERSION#v} ;;
    --no-toolchain) TOOLCHAIN=0 ;;
    --no-agents) AGENTS=0 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
  shift
done

# ask "question" → 0 for yes. Reads from the terminal even when piped (curl … | sh).
ask() {
  [ "$YES" = 1 ] && return 0
  if [ -r /dev/tty ] && [ -w /dev/tty ]; then
    printf '  %s?%s %s [Y/n] ' "$E" "$N" "$1" >/dev/tty
    read -r answer </dev/tty || answer=n
    case "$answer" in ""|y|Y|yes|YES) return 0 ;; *) return 1 ;; esac
  fi
  warn "$1: no terminal to ask on; skipping (re-run with --yes to accept)"
  return 1
}

# run cmd… : executes, or only prints in --dry-run
run() {
  if [ "$DRY" = 1 ]; then printf '  %s$ %s%s\n' "$D" "$*" "$N"; return 0; fi
  "$@"
}

sha256_of() {
  if have sha256sum; then sha256sum "$1" | cut -d' ' -f1
  elif have shasum; then shasum -a 256 "$1" | cut -d' ' -f1
  else die "need sha256sum or shasum to verify the download"; fi
}

# Ed25519 public key for release signatures (checksums.txt.sig, made by
# tools/release/release.sh with OFFRAILS_SIGN_KEY). The private key is kept by
# the maintainer outside this repo, never committed. OFFRAILS_PUBKEY_FILE
# overrides the key file (key rotation, tests).
RELEASE_PUBKEY="-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEA0O8r0OjSDxhkB6f5y4LudQu2jMg7qLEt7PrFiL90nlo=
-----END PUBLIC KEY-----"

verify_sums_signature() {
  pub=${OFFRAILS_PUBKEY_FILE:-}
  if [ -z "$pub" ]; then
    pub="$TMPD/release-pub.pem"
    printf '%s\n' "$RELEASE_PUBKEY" >"$pub"
  fi
  have openssl || die "need openssl to verify the release signature"
  openssl pkeyutl -verify -pubin -inkey "$pub" -rawin -in "$1" -sigfile "$2" >/dev/null 2>&1 \
    || die "release signature verification failed for $SUMS. Not installing."
  ok "release signature verified (Ed25519)"
}

# fetch <url-or-path> <dest>
fetch() {
  case "$1" in
    http://*|https://*)
      if have curl; then curl -fsSL --proto '=https' --tlsv1.2 "$1" -o "$2"
      elif have wget; then wget -q --https-only "$1" -O "$2"
      else die "need curl or wget to download $1"; fi ;;
    file://*) cp "${1#file://}" "$2" ;;
    *) cp "$1" "$2" ;;
  esac
}

# ── where the release comes from ──────────────────────────────────────────────
if [ -n "${OFFRAILS_SOURCE:-}" ]; then
  src=${OFFRAILS_SOURCE%/}
  case "$src" in
    *.tar.gz) TARBALL_URL=$src; SUMS_URL="$(dirname "$src")/$SUMS" ;;
    *) TARBALL_URL="$src/$ASSET"; SUMS_URL="$src/$SUMS" ;;
  esac
  SOURCE_DESC="$src (OFFRAILS_SOURCE)"
elif [ -n "$WANT_VERSION" ]; then
  TARBALL_URL="https://github.com/$REPO/releases/download/v$WANT_VERSION/$ASSET"
  SUMS_URL="https://github.com/$REPO/releases/download/v$WANT_VERSION/$SUMS"
  SOURCE_DESC="github.com/$REPO release v$WANT_VERSION"
else
  TARBALL_URL="https://github.com/$REPO/releases/latest/download/$ASSET"
  SUMS_URL="https://github.com/$REPO/releases/latest/download/$SUMS"
  SOURCE_DESC="github.com/$REPO latest release"
fi

case "$(uname -s)" in Darwin) OS=macOS ;; Linux) OS=Linux ;; *) OS=$(uname -s) ;; esac
ARCH=$(uname -m)

# ── plan ──────────────────────────────────────────────────────────────────────
say ""
say "  ${E}\\R/${N}  ${B}Rust off Rails${N} installer   ${D}the opinionated web stack for the AI era${N}"
say ""
step "Plan ($OS $ARCH)"
if [ "$TOOLCHAIN" = 1 ]; then
  note "Rust toolchain: $(have cargo && cargo --version 2>/dev/null || echo 'not found → offer rustup (official installer, no sudo)')"
  tools_missing=""; for t in sea-orm-cli cargo-nextest cargo-watch; do have "$t" || tools_missing="$tools_missing $t"; done
  if [ -n "$tools_missing" ]; then note "cargo tools to install:$tools_missing (prebuilt via cargo-binstall)"; else note "cargo tools: sea-orm-cli, cargo-nextest, cargo-watch (already installed)"; fi
else
  note "Rust toolchain: skipped (--no-toolchain)"
fi
note "framework: $SOURCE_DESC → $(pretty "$OFFRAILS_HOME")/<version> (SHA-256 verified)"
if [ "$AGENTS" = 1 ]; then
  note "Claude Code plugin: $(have claude && echo 'claude found → install offrails@offrails' || echo 'claude not found → print instructions')"
else
  note "Claude Code plugin: skipped (--no-agents)"
fi
note "command: $(pretty "$BIN_DIR")/offrails"
note "detect only (optional): Docker, Node 24 + pnpm"
say ""
if [ "$DRY" = 1 ]; then say "${D}Dry run: commands are printed, nothing is changed.${N}"; say ""; fi

# ── 1. toolchain ──────────────────────────────────────────────────────────────
if [ "$TOOLCHAIN" = 1 ]; then
  step "Rust toolchain"
  # shellcheck disable=SC1091 # written by rustup
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
  if have cargo; then
    ok "$(cargo --version)"
  elif ask "Rust isn't installed. Install it with rustup (https://rustup.rs)?"; then
    have curl || die "installing rustup needs curl"
    if [ "$DRY" = 1 ]; then
      run sh -c "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y"
    else
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y >/dev/null || die "rustup failed"
      # shellcheck disable=SC1091 # written by rustup
      . "$HOME/.cargo/env"
      ok "$(cargo --version)"
    fi
  else
    warn "skipping Rust: install it later from https://rustup.rs, then run: offrails update"
  fi

  if have cargo || [ "$DRY" = 1 ]; then
    missing=""
    for t in sea-orm-cli cargo-nextest cargo-watch; do
      if have "$t"; then ok "$t"; else missing="$missing $t"; fi
    done
    if [ -n "$missing" ]; then
      if ! have cargo-binstall; then
        note "installing cargo-binstall (prebuilt binaries; official script)"
        if [ "$DRY" = 1 ]; then
          run sh -c "curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash"
        elif have curl && have bash; then
          curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash >/dev/null 2>&1 \
            || warn "cargo-binstall install failed; falling back to cargo install"
        fi
      fi
      # shellcheck disable=SC2086 # $missing is a space-separated list of crate names on purpose
      if have cargo-binstall || [ "$DRY" = 1 ]; then
        run cargo binstall --no-confirm --quiet $missing || warn "cargo binstall failed for:$missing"
      fi
      for t in $missing; do
        have "$t" && { ok "$t"; continue; }
        [ "$DRY" = 1 ] && continue
        warn "building $t from source (cargo install --locked); this can take several minutes"
        cargo install --locked --quiet "$t" || warn "could not install $t"
        have "$t" && ok "$t"
      done
    fi
  fi
fi

# ── 2. framework ──────────────────────────────────────────────────────────────
step "Framework"
TMPD=$(mktemp -d 2>/dev/null || mktemp -d -t offrails)
trap 'rm -rf "$TMPD"' EXIT INT TERM
if [ "$DRY" = 1 ]; then
  run fetch "$TARBALL_URL" "$TMPD/$ASSET"
  run fetch "$SUMS_URL" "$TMPD/$SUMS"
  note "verify $SUMS.sig (Ed25519) and SHA-256, unpack to $OFFRAILS_HOME/<version>, point $OFFRAILS_HOME/current at it"
  VERSION_INSTALLED="<version>"
else
  if ! fetch "$TARBALL_URL" "$TMPD/$ASSET"; then
    if [ -z "${OFFRAILS_SOURCE:-}" ]; then
      die "could not download $TARBALL_URL. Check your connection, or see https://github.com/$REPO/releases (a release with $ASSET and $SUMS must exist)."
    fi
    die "could not read $TARBALL_URL"
  fi
  fetch "$SUMS_URL" "$TMPD/$SUMS" || die "could not download $SUMS_URL"
  if fetch "$SUMS_URL.sig" "$TMPD/$SUMS.sig" 2>/dev/null; then
    verify_sums_signature "$TMPD/$SUMS" "$TMPD/$SUMS.sig"
  else
    case "$TARBALL_URL" in
      http://*|https://*) die "no $SUMS.sig beside $SUMS_URL: refusing an unsigned download (releases are signed; see INSTALL.md)" ;;
      *) warn "no $SUMS.sig beside the local source; trusting the local source without a signature" ;;
    esac
  fi
  expected=$(awk -v f="$ASSET" '$2==f || $2=="*"f {print $1}' "$TMPD/$SUMS" | head -n1)
  [ -n "$expected" ] || die "$SUMS has no entry for $ASSET"
  actual=$(sha256_of "$TMPD/$ASSET")
  [ "$expected" = "$actual" ] || die "checksum mismatch for $ASSET (expected $expected, got $actual). Not installing."
  ok "SHA-256 verified ${D}$actual${N}"

  mkdir -p "$TMPD/x"
  # -m: the files get the time they were unpacked, newer than any build an app has cached
  tar -xmzf "$TMPD/$ASSET" -C "$TMPD/x" || die "could not unpack $ASSET"
  top=$(find "$TMPD/x" -mindepth 1 -maxdepth 1 -type d | head -n1)
  if [ ! -f "$top/VERSION" ] || [ ! -f "$top/AGENTS.md" ]; then
    die "$ASSET doesn't look like a Rust off Rails release"
  fi
  VERSION_INSTALLED=$(head -n1 "$top/VERSION")
  dest="$OFFRAILS_HOME/$VERSION_INSTALLED"
  mkdir -p "$OFFRAILS_HOME"
  if [ -f "$dest/.sha256" ] && [ "$(cat "$dest/.sha256")" = "$actual" ]; then
    ok "$VERSION_INSTALLED already installed at $(pretty "$dest")"
  else
    rm -rf "$dest.tmp"; mv "$top" "$dest.tmp"
    printf '%s\n' "$actual" >"$dest.tmp/.sha256"
    rm -rf "$dest"; mv "$dest.tmp" "$dest"
    ok "installed $VERSION_INSTALLED to $(pretty "$dest")"
  fi
  ( cd "$OFFRAILS_HOME" && ln -sfn "$VERSION_INSTALLED" current )
  ok "$(pretty "$OFFRAILS_HOME")/current → $VERSION_INSTALLED"
fi

# ── 3. the offrails command ───────────────────────────────────────────────────
step "The offrails command"
if [ "$DRY" = 1 ]; then
  run cp "$OFFRAILS_HOME/current/tools/offrails" "$BIN_DIR/offrails"
else
  mkdir -p "$BIN_DIR"
  if [ -f "$BIN_DIR/offrails" ] && cmp -s "$OFFRAILS_HOME/current/tools/offrails" "$BIN_DIR/offrails"; then
    ok "$(pretty "$BIN_DIR")/offrails is up to date"
  else
    cp "$OFFRAILS_HOME/current/tools/offrails" "$BIN_DIR/offrails.tmp" && chmod 755 "$BIN_DIR/offrails.tmp" && mv "$BIN_DIR/offrails.tmp" "$BIN_DIR/offrails"
    ok "installed $(pretty "$BIN_DIR")/offrails"
  fi
fi
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) warn "$BIN_DIR is not on your PATH. Add this line to your shell profile (~/.zshrc or ~/.bashrc):"
     say "      export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

# ── 4. agents ─────────────────────────────────────────────────────────────────
step "Coding agents"
if [ "$AGENTS" = 1 ] && have claude; then
  if claude plugin list --json 2>/dev/null | grep -q '"offrails@offrails"'; then
    ok "Claude Code plugin offrails@offrails already installed (update with: claude plugin update offrails@offrails)"
  else
    # The release tarball above is checksum-verified at $VERSION_INSTALLED, and the plugin is
    # installed from that verified copy on disk, so it is pinned to the same release.
    note "the Claude Code plugin is installed from the checksum-verified framework copy at $(pretty "$OFFRAILS_HOME/current") (release $VERSION_INSTALLED), not from the moving default branch"
    if ! claude plugin marketplace list --json 2>/dev/null | grep -q '"name": *"offrails"'; then
      run claude plugin marketplace add "$OFFRAILS_HOME/current" >/dev/null 2>&1 || warn "could not add the marketplace at $OFFRAILS_HOME/current (try: claude plugin marketplace add $OFFRAILS_HOME/current)"
    fi
    if run claude plugin install offrails@offrails --scope user >/dev/null 2>&1; then
      [ "$DRY" = 1 ] || ok "Claude Code plugin installed: /offrails:new, /offrails:feature, … (restart Claude Code to load it)"
    else
      warn "plugin install failed; inside Claude Code run: /plugin marketplace add $OFFRAILS_HOME/current, then /plugin install offrails@offrails"
    fi
  fi
elif [ "$AGENTS" = 1 ]; then
  note "Claude Code not found. With it (recommended): https://claude.com/claude-code, then: offrails update"
fi
note "Codex, Cursor or any agent: ${B}Read $(pretty "$OFFRAILS_HOME")/current/AGENTS.md and make me a new app called bookshelf${N}"

# ── 5. optional tools ─────────────────────────────────────────────────────────
step "Optional tools"
if have docker; then ok "Docker $(docker --version 2>/dev/null | sed 's/^Docker version //; s/,.*//')"; else note "Docker not found: optional (local Postgres, Docker builds, deploys)"; fi
if have node; then
  nv=$(node --version 2>/dev/null | sed 's/^v//'); major=${nv%%.*}
  if [ "${major:-0}" -ge 24 ] 2>/dev/null; then ok "Node $nv"; else warn "Node $nv: inertia/spa/islands apps need Node 24 LTS"; fi
else
  note "Node not found: only needed for inertia/spa/islands apps (Node 24 LTS)"
fi
if have pnpm; then ok "pnpm $(pnpm --version 2>/dev/null)"; else note "pnpm not found: comes with Node via 'corepack enable pnpm' (only for JS frontends)"; fi

# ── done ──────────────────────────────────────────────────────────────────────
say ""
if [ "$DRY" = 1 ]; then
  say "Dry run complete. Run without --dry-run to install."
else
  say "${G}${B}Rust off Rails $VERSION_INSTALLED is ready.${N}"
  say ""
  say "  Start an app:   ${B}offrails new bookshelf \"people track the books they've read\"${N}"
  say "  Check setup:    offrails doctor"
  say "  Learn:          https://rustoffrails.org/start"
fi
say ""
