#!/bin/sh
# Build and publish a Rust off Rails release.
#
#   tools/release/release.sh                 build from HEAD and create GitHub release v$(cat VERSION)
#   tools/release/release.sh --dry-run      build, print what would be published, publish nothing
#   tools/release/release.sh --build-only --out DIR [--worktree]
#                                            just build the assets (tests use --worktree to include
#                                            uncommitted files)
#
# Assets (stable names so https://github.com/<repo>/releases/latest/download/<asset> works):
#   rustoffrails.tar.gz   the whole framework under rustoffrails/, skills and hooks included
#                         (~/.offrails serves Codex and other agents, not only the plugin)
#   install.sh            the installer, also served at https://rustoffrails.org/install.sh
#   checksums.txt         SHA-256 of both, verified by install.sh
set -eu

cd "$(dirname "$0")/../.."
REPO="rust-off-rails/rustoffrails"
DRY=0; BUILD_ONLY=0; WORKTREE=0; OUT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY=1 ;;
    --build-only) BUILD_ONLY=1 ;;
    --worktree) WORKTREE=1 ;;
    --out) OUT=$2; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

VERSION=$(head -n1 VERSION)
TAG="v$VERSION"
OUT=${OUT:-$(mktemp -d)}
mkdir -p "$OUT"

if [ "$WORKTREE" = 0 ] && [ -n "$(git status --porcelain)" ]; then
  echo "release: working tree is dirty; commit first (or use --build-only --worktree for tests)." >&2
  echo "  Untracked local files (such as .claude/) count too; release from a clean worktree:" >&2
  echo "    git worktree add --detach ../rel HEAD && cd ../rel && tools/release/release.sh" >&2
  exit 1
fi
plugin_version=$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' .claude-plugin/plugin.json | head -n1)
[ "$plugin_version" = "$VERSION" ] || { echo "release: VERSION ($VERSION) and plugin.json ($plugin_version) disagree" >&2; exit 1; }
# offrails-gen's version is part of Cargo's fingerprint: a new one rebuilds an app's cached binary
# (target/offrails-gen) even when a copy kept older file times.
gen_version=$(sed -n 's/^version = "\([^"]*\)".*/\1/p' tools/offrails-gen/Cargo.toml | head -n1)
[ "$gen_version" = "$VERSION" ] || { echo "release: VERSION ($VERSION) and tools/offrails-gen/Cargo.toml ($gen_version) disagree" >&2; exit 1; }

# Deterministic tarball: sorted paths, fixed owner, every mtime the release commit's time (not a
# fixed old date: Cargo compares file times with its last build, and files older than an app's
# cached offrails-gen binary made it reuse the previous release's), gzip without a timestamp.
stage=$(mktemp -d); trap 'rm -rf "$stage"' EXIT INT TERM
mkdir -p "$stage/rustoffrails"
if [ "$WORKTREE" = 1 ]; then
  git ls-files -co --exclude-standard | while IFS= read -r f; do
    [ -e "$f" ] || continue
    mkdir -p "$stage/rustoffrails/$(dirname "$f")"; cp -p "$f" "$stage/rustoffrails/$f"
  done
else
  git archive --format=tar HEAD | tar -xf - -C "$stage/rustoffrails"
fi
stamp=$(TZ=UTC0 git log -1 --format=%cd --date=format-local:%Y%m%d%H%M.%S HEAD)
TZ=UTC0 find "$stage" -exec touch -h -t "$stamp" {} + 2>/dev/null || TZ=UTC0 find "$stage" -exec touch -t "$stamp" {} +
if tar --version 2>/dev/null | grep -q GNU; then
  (cd "$stage" && tar --sort=name --owner=0 --group=0 --numeric-owner -cf - rustoffrails) | gzip -n >"$OUT/rustoffrails.tar.gz"
else
  # bsdtar (macOS): no extended attributes or AppleDouble files, which GNU tar on Linux warns about
  (cd "$stage" && find rustoffrails -print | LC_ALL=C sort | COPYFILE_DISABLE=1 tar --uid 0 --gid 0 --no-xattrs --no-mac-metadata -cf - -n -T -) | gzip -n >"$OUT/rustoffrails.tar.gz"
fi
cp install.sh "$OUT/install.sh"
(cd "$OUT" && { if command -v sha256sum >/dev/null; then sha256sum rustoffrails.tar.gz install.sh; else shasum -a 256 rustoffrails.tar.gz install.sh; fi; } >checksums.txt)

echo "Built Rust off Rails $VERSION in $OUT:"
for f in rustoffrails.tar.gz install.sh checksums.txt; do printf '  %s  %s bytes\n' "$f" "$(wc -c <"$OUT/$f" | tr -d ' ')"; done
[ "$BUILD_ONLY" = 1 ] && exit 0

# The site keeps serving the previous release until it is updated (#25).
website_steps() {
  echo "Then update rustoffrails.org, which serves /install.sh, /docs, /changelog and the MCP answers"
  echo "from its own copy of the release:"
  echo "  1. re-vendor .offrails/ in the website repo from $TAG"
  echo "  2. bin/releases add $VERSION"
  echo "  3. deploy, then check https://rustoffrails.org/install.sh and /changelog name $VERSION"
}

notes=$(awk -v v="$VERSION" '$0 ~ "^## "v" " {f=1; next} f && /^## / {exit} f' CHANGELOG.md)
[ -n "$notes" ] || { echo "release: CHANGELOG.md has no '## $VERSION (…)' section" >&2; exit 1; }
if [ "$DRY" = 1 ]; then
  echo "Dry run. Would run:"
  echo "  git tag -a $TAG -m 'Rust off Rails $VERSION' && git push origin $TAG"
  echo "  gh release create $TAG $OUT/rustoffrails.tar.gz $OUT/install.sh $OUT/checksums.txt --repo $REPO --title 'Rust off Rails $VERSION' --notes <CHANGELOG section, $(printf '%s' "$notes" | wc -l | tr -d ' ') lines>"
  website_steps
  exit 0
fi
git tag -a "$TAG" -m "Rust off Rails $VERSION"
git push origin "$TAG"
printf '%s\n' "$notes" | gh release create "$TAG" "$OUT/rustoffrails.tar.gz" "$OUT/install.sh" "$OUT/checksums.txt" \
  --repo "$REPO" --title "Rust off Rails $VERSION" --notes-file -
echo "Released $TAG: https://github.com/$REPO/releases/tag/$TAG"
website_steps
