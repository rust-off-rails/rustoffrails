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
#   checksums.txt.sig     Ed25519 signature of checksums.txt (OFFRAILS_SIGN_KEY, a PEM private
#                         key held offline, never in this repo). Publishing requires it;
#                         --build-only signs only when the key is set (tests use a throwaway).
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

if [ "$BUILD_ONLY" = 0 ] && [ "$DRY" = 0 ] && { [ -z "${OFFRAILS_SIGN_KEY:-}" ] || [ ! -f "$OFFRAILS_SIGN_KEY" ]; }; then
  echo "release: OFFRAILS_SIGN_KEY must name the Ed25519 private key (PEM) that signs checksums.txt; publishing an unsigned release is refused (install.sh requires the signature for downloads)" >&2
  exit 1
fi

VERSION=$(head -n1 VERSION)
TAG="v$VERSION"
OUT=${OUT:-$(mktemp -d)}
mkdir -p "$OUT"

if [ "$WORKTREE" = 0 ] && [ -n "$(git status --porcelain)" ]; then
  echo "release: working tree is dirty; commit first (or use --build-only --worktree for tests)" >&2
  exit 1
fi
plugin_version=$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' .claude-plugin/plugin.json | head -n1)
[ "$plugin_version" = "$VERSION" ] || { echo "release: VERSION ($VERSION) and plugin.json ($plugin_version) disagree" >&2; exit 1; }

# Deterministic tarball: sorted paths, fixed owner and mtime, gzip without a timestamp.
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
find "$stage" -exec touch -h -t 200001010000 {} + 2>/dev/null || find "$stage" -exec touch -t 200001010000 {} +
if tar --version 2>/dev/null | grep -q GNU; then
  (cd "$stage" && tar --sort=name --owner=0 --group=0 --numeric-owner --mtime='2000-01-01' -cf - rustoffrails) | gzip -n >"$OUT/rustoffrails.tar.gz"
else
  # bsdtar (macOS): no extended attributes or AppleDouble files, which GNU tar on Linux warns about
  (cd "$stage" && find rustoffrails -print | LC_ALL=C sort | COPYFILE_DISABLE=1 tar --uid 0 --gid 0 --no-xattrs --no-mac-metadata -cf - -n -T -) | gzip -n >"$OUT/rustoffrails.tar.gz"
fi
cp install.sh "$OUT/install.sh"
(cd "$OUT" && { if command -v sha256sum >/dev/null; then sha256sum rustoffrails.tar.gz install.sh; else shasum -a 256 rustoffrails.tar.gz install.sh; fi; } >checksums.txt)
if [ -n "${OFFRAILS_SIGN_KEY:-}" ] && [ -f "$OFFRAILS_SIGN_KEY" ]; then
  openssl pkeyutl -sign -rawin -inkey "$OFFRAILS_SIGN_KEY" -in "$OUT/checksums.txt" -out "$OUT/checksums.txt.sig" \
    || { echo "release: signing checksums.txt with OFFRAILS_SIGN_KEY failed" >&2; exit 1; }
fi

echo "Built Rust off Rails $VERSION in $OUT:"
for f in rustoffrails.tar.gz install.sh checksums.txt checksums.txt.sig; do [ -f "$OUT/$f" ] && printf '  %s  %s bytes\n' "$f" "$(wc -c <"$OUT/$f" | tr -d ' ')"; done
[ "$BUILD_ONLY" = 1 ] && exit 0

notes=$(awk -v v="$VERSION" '$0 ~ "^## "v" " {f=1; next} f && /^## / {exit} f' CHANGELOG.md)
[ -n "$notes" ] || { echo "release: CHANGELOG.md has no '## $VERSION (…)' section" >&2; exit 1; }
if [ "$DRY" = 1 ]; then
  echo "Dry run. Would run:"
  echo "  git tag -a $TAG -m 'Rust off Rails $VERSION' && git push origin $TAG"
  echo "  gh release create $TAG $OUT/rustoffrails.tar.gz $OUT/install.sh $OUT/checksums.txt $OUT/checksums.txt.sig --repo $REPO --title 'Rust off Rails $VERSION' --notes <CHANGELOG section, $(printf '%s' "$notes" | wc -l | tr -d ' ') lines>"
  echo "  then publish install.sh at https://rustoffrails.org/install.sh (the website serves the latest release's copy)"
  exit 0
fi
git tag -a "$TAG" -m "Rust off Rails $VERSION"
git push origin "$TAG"
printf '%s\n' "$notes" | gh release create "$TAG" "$OUT/rustoffrails.tar.gz" "$OUT/install.sh" "$OUT/checksums.txt" "$OUT/checksums.txt.sig" \
  --repo "$REPO" --title "Rust off Rails $VERSION" --notes-file -
echo "Released $TAG: https://github.com/$REPO/releases/tag/$TAG"
