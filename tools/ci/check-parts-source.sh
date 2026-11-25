#!/bin/sh
# template/parts/ must be exactly what tools/sync-parts.sh writes from the samples commit named in
# template/parts/SOURCE: parts are written in rust-off-rails/samples, never edited here (#28).
#
#   tools/ci/check-parts-source.sh [<samples-checkout>]   (default: a fresh clone of the samples repo)
#
# It re-runs the sync into this checkout, compares, and restores template/parts/ afterwards, so it
# refuses to start when template/parts/ has uncommitted changes. SOURCE itself is not compared: its
# `describe` text depends on the tags the samples checkout has.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
if [ -n "$(git status --porcelain -- template/parts)" ]; then
  echo "check-parts-source: template/parts/ has uncommitted changes; commit or discard them first" >&2
  exit 2
fi

commit=$(awk '{ print $2 }' template/parts/SOURCE)
case "$commit" in
  [0-9a-f]*) ;;
  *) echo "check-parts-source: template/parts/SOURCE names no commit" >&2; exit 1 ;;
esac

samples=${1:-}
tmp=$(mktemp -d)
trap 'git checkout -q -- template/parts; git clean -qfd -- template/parts; rm -rf "$tmp"' EXIT INT TERM
if [ -z "$samples" ]; then
  samples="$tmp/samples"
  git clone -q https://github.com/rust-off-rails/samples.git "$samples"
fi
git -C "$samples" cat-file -e "$commit^{commit}" 2>/dev/null || {
  echo "check-parts-source: samples commit $commit (template/parts/SOURCE) is not in $samples" >&2
  exit 1
}
if ! git -C "$samples" merge-base --is-ancestor "$commit" origin/main 2>/dev/null &&
  ! git -C "$samples" merge-base --is-ancestor "$commit" main 2>/dev/null; then
  echo "check-parts-source: samples commit $commit is not on samples main; sync from a merged commit" >&2
  exit 1
fi

tools/sync-parts.sh "$samples" "$commit" >/dev/null
drift=$(git status --porcelain -- template/parts | grep -v ' template/parts/SOURCE$' || true)
if [ -n "$drift" ]; then
  echo "check-parts-source: template/parts/ differs from samples $commit:" >&2
  printf '%s\n' "$drift" >&2
  echo "Make the change in rust-off-rails/samples, merge it, then: tools/sync-parts.sh <samples checkout> <commit>" >&2
  exit 1
fi
echo "check-parts-source: template/parts/ matches samples $commit"
