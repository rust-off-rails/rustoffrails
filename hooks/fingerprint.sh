#!/usr/bin/env bash
# Usage: fingerprint.sh <framework dir>
# Prints a content hash of the framework docs: every file except plugin-only
# directories, VERSION and the installer's .sha256 (the tarball checksum it writes
# into ~/.offrails/<version>/, which the plugin's copy doesn't have). Use it to compare an app's .offrails/ with the plugin,
# so drift is detected even when the version number didn't change.
set -euo pipefail
dir="${1:?framework dir}"
if command -v sha256sum >/dev/null 2>&1; then sha() { sha256sum; }; else sha() { shasum -a 256; }; fi
cd "$dir"
find . -type f \
  -not -path './.git' -not -path './.git/*' -not -path './.claude-plugin/*' -not -path './.in_use/*' -not -path './skills/*' \
  -not -path './hooks/*' -not -path './evals/*' -not -path './brand/*' -not -name VERSION -not -path './.sha256' -not -name '.DS_Store' -not -path './tools/offrails-gen/target/*' -not -path './template/app/target/*' -not -path './media/*' \
  | LC_ALL=C sort \
  | while IFS= read -r f; do printf '%s  %s\n' "$(sha <"$f" | cut -c1-64)" "$f"; done \
  | sha | cut -c1-16
