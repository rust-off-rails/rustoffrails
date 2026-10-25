#!/bin/sh
# shellcheck disable=SC2015 # pass() never fails, so A && pass || bad is safe here
# Installer tests: builds the release assets from the working tree, then installs them
# (OFFRAILS_SOURCE, --yes) into a temp HOME on this machine and into clean Linux containers
# as a non-root user. Asserts the files land, the `offrails` command works, re-running is a
# no-op, and a tampered checksum is refused.
#
#   tools/ci/test-install.sh            local + containers (skips containers without Docker)
#   tools/ci/test-install.sh --full     also runs the real toolchain path (rustup + cargo-binstall)
#                                       in ubuntu:24.04: slow, needs network
set -eu
cd "$(dirname "$0")/../.."
FULL=0; [ "${1:-}" = "--full" ] && FULL=1
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT INT TERM
fail=0
pass() { echo "ok   $*"; }
bad()  { echo "FAIL $*"; fail=1; }

tools/release/release.sh --build-only --worktree --out "$T/dist" >/dev/null
VERSION=$(head -n1 VERSION)

# ── local (this machine), with every POSIX shell available ──
for shell in sh dash bash zsh; do
  command -v "$shell" >/dev/null 2>&1 || continue
  H="$T/home-$shell"; mkdir -p "$H"
  if HOME="$H" OFFRAILS_SOURCE="$T/dist" NO_COLOR=1 "$shell" install.sh --yes --no-toolchain --no-agents >"$T/out-$shell" 2>&1; then
    [ -f "$H/.offrails/$VERSION/AGENTS.md" ] && [ -L "$H/.offrails/current" ] && [ -x "$H/.local/bin/offrails" ] \
      && pass "local/$shell: install lands framework, current symlink and command" || bad "local/$shell: files missing"
    HOME="$H" "$H/.local/bin/offrails" version | grep -q "Rust off Rails $VERSION" && pass "local/$shell: offrails version" || bad "local/$shell: offrails version"
    HOME="$H" OFFRAILS_SOURCE="$T/dist" NO_COLOR=1 "$shell" install.sh --yes --no-toolchain --no-agents 2>&1 | grep -q "already installed" \
      && pass "local/$shell: re-run is a no-op" || bad "local/$shell: re-run not idempotent"
  else
    bad "local/$shell: install failed"; cat "$T/out-$shell"
  fi
done

H="$T/home-doctor"; mkdir -p "$H"
HOME="$H" OFFRAILS_SOURCE="$T/dist" sh install.sh --yes --no-toolchain --no-agents >/dev/null 2>&1
HOME="$H" "$H/.local/bin/offrails" doctor >"$T/doctor" 2>&1 || true
grep -q "framework      ok" "$T/doctor" && pass "local: offrails doctor reports the framework" || { bad "local: offrails doctor"; cat "$T/doctor"; }
for f in template/README.md template/app/Cargo.toml template/app/Cargo.lock template/app/.cargo/config.toml template/parts/mailer/sample.toml template/parts/SOURCE tools/offrails-gen/Cargo.toml tools/offrails-gen/Cargo.lock tools/offrails-gen/src/main.rs; do
  [ -f "$H/.offrails/$VERSION/$f" ] || { bad "local: the release has no $f"; continue; }
done
[ -d "$H/.offrails/$VERSION/tools/offrails-gen/target" ] && bad "local: the release carries tools/offrails-gen/target" || pass "local: the release carries the template, its parts and offrails-gen (no build output)"
HOME="$H" "$H/.local/bin/offrails" new Bad_Name >/dev/null 2>&1 && bad "local: invalid app name accepted" || pass "local: invalid app name rejected"
# upgrade --check: against the installed framework, and against another one with --fw (a tag export)
A="$T/app"; mkdir -p "$A"; cp -R "$H/.offrails/current/" "$A/.offrails"
(cd "$A" && HOME="$H" "$H/.local/bin/offrails" upgrade --check >"$T/up1" 2>&1) \
  && grep -q "Up to date" "$T/up1" && pass "local: upgrade --check, app current" || { bad "local: upgrade --check (current)"; cat "$T/up1"; }
cp -R "$H/.offrails/current/" "$T/export"; echo "changed" >>"$T/export/workflow.md"
set +e; (cd "$A" && HOME="$H" "$H/.local/bin/offrails" upgrade --check --fw "$T/export" >"$T/up2" 2>&1); rc=$?; set -e
[ "$rc" = 10 ] && grep -q "installed framework" "$T/up2" && pass "local: upgrade --check --fw <dir> sees a different framework (exit 10)" || { bad "local: upgrade --check --fw (rc=$rc)"; cat "$T/up2"; }
(cd "$A" && HOME="$H" "$H/.local/bin/offrails" upgrade --check --fw "$T/nope" >/dev/null 2>&1) && bad "local: --fw with a missing dir accepted" || pass "local: --fw with a missing dir refused"
# --help / -h never runs anything: fake agents on PATH drop a marker if they are ever started.
FAKE="$T/fake-bin"; MARK="$T/agent-ran"; mkdir -p "$FAKE"
for a in claude opencode codex; do printf '#!/bin/sh\necho "%s $*" >>"%s"\n' "$a" "$MARK" >"$FAKE/$a"; chmod +x "$FAKE/$a"; done
OR() { (cd "$A" && HOME="$H" PATH="$FAKE:$PATH" "$H/.local/bin/offrails" "$@" </dev/null); }
for sub in new upgrade check doctor version update uninstall; do
  for flag in --help -h; do
    rm -f "$MARK"; set +e; OR "$sub" "$flag" >"$T/help" 2>&1; rc=$?; set -e
    [ "$rc" = 0 ] && grep -q "Usage: offrails $sub" "$T/help" && [ ! -e "$MARK" ] && [ -e "$H/.offrails" ] \
      && pass "local: offrails $sub $flag prints help, runs nothing" || { bad "local: offrails $sub $flag (rc=$rc, agent: $(cat "$MARK" 2>/dev/null))"; cat "$T/help"; }
  done
done
rm -f "$MARK"; set +e; OR upgrade --fw "$T/export" --help >/dev/null 2>&1; rc=$?; set -e
[ "$rc" = 0 ] && [ ! -e "$MARK" ] && pass "local: --help after other flags still runs nothing" || bad "local: upgrade --fw … --help (rc=$rc)"
for args in "upgrade --frob" "new --frob x" "check --frob" "doctor --frob" "update --frob" "uninstall --frob" "frob"; do
  rm -f "$MARK"; set +e; eval "OR $args" >/dev/null 2>&1; rc=$?; set -e
  [ "$rc" = 2 ] && [ ! -e "$MARK" ] && [ -e "$H/.offrails" ] && pass "local: offrails $args → usage, exit 2, nothing run" || bad "local: offrails $args (rc=$rc)"
done
rm -f "$MARK"; set +e; OR upgrade --fw "$T/export" >"$T/up3" 2>&1; rc=$?; set -e
[ "$rc" = 3 ] && [ ! -e "$MARK" ] && grep -q "Would run: claude /offrails:upgrade" "$T/up3" \
  && pass "local: non-interactive upgrade without --yes prints the command, exit 3, no agent" || { bad "local: upgrade without --yes (rc=$rc)"; cat "$T/up3"; }
rm -f "$MARK"; set +e; OR new notes >/dev/null 2>&1; rc=$?; set -e
[ "$rc" = 3 ] && [ ! -e "$MARK" ] && pass "local: non-interactive new without --yes, no agent" || bad "local: new without --yes (rc=$rc)"
rm -f "$MARK"; OR upgrade --fw "$T/export" --yes >/dev/null 2>&1 || true
grep -q "^claude /offrails:upgrade" "$MARK" 2>/dev/null && pass "local: upgrade --yes launches the agent" || bad "local: upgrade --yes did not launch the agent"
rm -f "$MARK"; (cd "$A" && HOME="$H" PATH="$FAKE:$PATH" OFFRAILS_YES=1 OFFRAILS_AGENT=codex "$H/.local/bin/offrails" upgrade --fw "$T/export" </dev/null >/dev/null 2>&1) || true
grep -q "^codex Upgrade this" "$MARK" 2>/dev/null && pass "local: OFFRAILS_YES=1 launches the chosen agent" || bad "local: OFFRAILS_YES=1"

HOME="$H" "$H/.local/bin/offrails" uninstall -y >/dev/null 2>&1; [ ! -e "$H/.offrails" ] && [ ! -e "$H/.local/bin/offrails" ] && pass "local: uninstall removes everything" || bad "local: uninstall"

mkdir -p "$T/tampered"; cp "$T/dist/"* "$T/tampered/"
awk '$2=="rustoffrails.tar.gz"{$1="0000000000000000000000000000000000000000000000000000000000000000"} {print $1"  "$2}' "$T/dist/checksums.txt" >"$T/tampered/checksums.txt"
H="$T/home-tamper"; mkdir -p "$H"
if HOME="$H" OFFRAILS_SOURCE="$T/tampered" sh install.sh --yes --no-toolchain --no-agents >"$T/tamper" 2>&1; then bad "local: tampered checksum was installed"
else grep -q "checksum mismatch" "$T/tamper" && [ ! -e "$H/.offrails/$VERSION" ] && pass "local: tampered checksum refused, nothing installed" || bad "local: tamper message"; fi

HOME="$T/home-dry" OFFRAILS_SOURCE="$T/dist" sh install.sh --dry-run --no-agents >/dev/null 2>&1 && [ ! -e "$T/home-dry/.offrails" ] && pass "local: --dry-run changes nothing" || bad "local: --dry-run"

# A signed dist (throwaway Ed25519 key) verifies; the same dist with a swapped
# checksums.txt is refused before anything is installed.
openssl genpkey -algorithm Ed25519 -out "$T/sign.pem" 2>/dev/null
openssl pkey -in "$T/sign.pem" -pubout -out "$T/sign.pub.pem" 2>/dev/null
OFFRAILS_SIGN_KEY="$T/sign.pem" sh tools/release/release.sh --build-only --worktree --out "$T/dist-signed" >/dev/null 2>&1
[ -f "$T/dist-signed/checksums.txt.sig" ] && pass "local: release signs checksums.txt" || bad "local: release signing"
H="$T/home-signed"; mkdir -p "$H"
HOME="$H" OFFRAILS_SOURCE="$T/dist-signed" OFFRAILS_PUBKEY_FILE="$T/sign.pub.pem" sh install.sh --yes --no-toolchain --no-agents >"$T/signed" 2>&1 \
  && [ -x "$H/.offrails/current/tools/offrails" ] \
  && pass "local: signed dist installs" || { bad "local: signed dist"; cat "$T/signed"; }
perl -i -pe 's/^./X/ if $. == 1' "$T/dist-signed/checksums.txt"
H="$T/home-forged"; mkdir -p "$H"
if HOME="$H" OFFRAILS_SOURCE="$T/dist-signed" OFFRAILS_PUBKEY_FILE="$T/sign.pub.pem" sh install.sh --yes --no-toolchain --no-agents >"$T/forged" 2>&1; then
  bad "local: forged checksums accepted"
elif [ ! -e "$H/.offrails" ] && grep -qi "signature" "$T/forged"; then
  pass "local: forged checksums refused, nothing installed"
else
  bad "local: forged checksums handling"; cat "$T/forged"
fi

# The plugin comes from the verified framework copy on disk, never the moving repo branch.
FAKEBIN="$T/fakebin"; mkdir -p "$FAKEBIN"
printf '#!/bin/sh\necho "claude $*" >>"%s/plugin-calls"\n' "$T" >"$FAKEBIN/claude"; chmod +x "$FAKEBIN/claude"
H="$T/home-plugin"; mkdir -p "$H"
HOME="$H" PATH="$FAKEBIN:/usr/bin:/bin" OFFRAILS_SOURCE="$T/dist" sh install.sh --yes --no-toolchain >"$T/plugin" 2>&1 || true
grep -q "plugin marketplace add $H/.offrails/current" "$T/plugin-calls" 2>/dev/null \
  && ! grep -q "rust-off-rails/rustoffrails" "$T/plugin-calls" \
  && pass "local: plugin marketplace is the verified local copy" || { bad "local: plugin source"; cat "$T/plugin-calls" 2>/dev/null; cat "$T/plugin"; }

# ── clean Linux containers, non-root ──
if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  for img in ubuntu:24.04 debian:trixie-slim; do
    if docker run --rm -v "$T/dist:/dist:ro" -v "$PWD/install.sh:/install.sh:ro" "$img" sh -ec '
        useradd -m dev
        su dev -c "OFFRAILS_SOURCE=/dist NO_COLOR=1 sh /install.sh --yes --no-toolchain --no-agents >/tmp/1 2>&1 || { cat /tmp/1; exit 1; }
                   ~/.local/bin/offrails version | grep -q \"Rust off Rails\"
                   ~/.local/bin/offrails doctor >/tmp/d 2>&1 || true; grep -q \"framework      ok\" /tmp/d
                   OFFRAILS_SOURCE=/dist sh /install.sh --yes --no-toolchain --no-agents 2>&1 | grep -q \"already installed\""
      ' >"$T/c" 2>&1; then pass "container/$img: install, command, doctor, idempotent re-run (non-root)"
    else bad "container/$img"; cat "$T/c"; fi
  done
  if [ "$FULL" = 1 ]; then
    if docker run --rm -v "$T/dist:/dist:ro" -v "$PWD/install.sh:/install.sh:ro" ubuntu:24.04 sh -ec '
        apt-get update -qq && apt-get install -y -qq curl ca-certificates build-essential bash >/dev/null
        useradd -m dev
        su dev -c "OFFRAILS_SOURCE=/dist NO_COLOR=1 sh /install.sh --yes --no-agents && . ~/.cargo/env && sea-orm-cli --version && cargo nextest --version && cargo watch --version && ~/.local/bin/offrails doctor"
      ' >"$T/full" 2>&1; then pass "container/ubuntu:24.04 --full: rustup + cargo-binstall toolchain"; tail -20 "$T/full"
    else bad "container/ubuntu:24.04 --full"; tail -40 "$T/full"; fi
  fi
else
  echo "skip containers: Docker not available"
fi

exit $fail
