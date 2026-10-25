#!/bin/sh
# Prove the app template and its parts the way an app gets them: generate with offrails-gen, then
# the Definition of Done on SQLite and Postgres.
#
#   tools/ci/test-template.sh                 gen, bare, all, incremental, generators, snippets, update-part
#   tools/ci/test-template.sh bare all        just these
#   tools/ci/test-template.sh --no-postgres   skip the Postgres runs (said so in the summary)
#
#   gen          offrails-gen's own tests and clippy
#   bare         `new` with no parts: `cargo offrails verify` (fmt, clippy, tests on SQLite + Postgres),
#                the no-unwrap lint, offrails-check, memory budget, and the boosted-navigation
#                browser smoke against the served app (skipped, and said so, without Chrome)
#   all          `new --parts all` (every part, so every sample's anchors are proven on the template;
#                the browser smoke signs in as the demo account too)
#   incremental  `new` bare, then `add-part` one part at a time into the generated app, then every
#                add-part again (must change nothing), then the same gates as `all`
#   generators   `cargo offrails generate` every kind into a bare app (--public: a references chain, a
#                resource with every field type, page, controller, migrations) and into an app with
#                every part (scoped resources, api, job, mailer, channel, model, test), the gates on
#                both databases, re-running each generator (a no-op), then `destroy` of all of it in
#                reverse: the app must be byte-for-byte what it was
#   snippets     every snippet (template/snippets/) added into a bare app and into an app with every
#                part, in dependency order (resource twice: as notes and --as article), the gates on
#                both databases, re-adding each (a no-op), then `snippet remove` of all of it in
#                reverse: the app must be byte-for-byte what it was
#   prove        `cargo offrails prove` on the bare app and the app with every part, each served in
#                development with a fixture feature whose acceptance criteria cite request tests:
#                PROOF.md says PROVEN, has every screenshot and maps each criterion to its passing
#                tests and pages; a second run diffs against the first; the component smoke passes
#                (both skipped, and said so, without Node or Chrome)
#   update-part  a synthetic newer part (a changed file and a changed wire) merged into an app: a
#                clean merge with an app edit elsewhere, then a conflict with an app edit on the
#                same line (markers, exit 1, nothing else touched)
#
# Postgres: TEST_DATABASE_URL names a server where the suite may create databases (a template and
# one reusable slot per concurrent test, testing.md#postgres-test-databases).
# Apps are generated in $OFFRAILS_CI_DIR (default: a temp dir), sharing one CARGO_TARGET_DIR.
# Exit status: 0 when every run passed.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
postgres=1
targets=""
for arg in "$@"; do
  case "$arg" in
    --no-postgres) postgres=0 ;;
    -h | --help) sed -n '2,38p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "test-template: unknown option $arg" >&2; exit 2 ;;
    *) targets="$targets $arg" ;;
  esac
done
[ -n "$targets" ] || targets="gen bare all incremental generators snippets prove update-part"
if [ "$postgres" = 1 ] && [ -z "${TEST_DATABASE_URL:-}" ]; then
  echo "test-template: TEST_DATABASE_URL is not set (--no-postgres skips the Postgres runs)" >&2
  exit 2
fi
pg_url="${TEST_DATABASE_URL:-}"
# OFFRAILS_CI_TAG: a suffix for the apps' crate names, so runs sharing a Postgres server (another
# checkout's CI on the same machine) never share test databases (they are named after the crate).
tag="${OFFRAILS_CI_TAG:-}"
# OFFRAILS_CI_PORT: the first of the four ports the browser smokes serve on (default 3911).
smoke_port="${OFFRAILS_CI_PORT:-3911}"
unset TEST_DATABASE_URL

work=${OFFRAILS_CI_DIR:-$(mktemp -d)}
mkdir -p "$work"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$work/target}"
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-never}"
gen_manifest="$root/tools/offrails-gen/Cargo.toml"
# `new --worktree`: this checkout's working tree as it is (uncommitted changes are what CI tests),
# not HEAD's committed files, which is what `new` vendors from a git checkout otherwise.
gen() {
  if [ "$1" = new ]; then set -- "$@" --worktree; fi
  cargo run -q --manifest-path "$gen_manifest" --target-dir "$work/gen-target" -- "$@"
}

name=""
app=""
step() {
  label=$1
  shift
  printf '\n== %s: %s\n' "$name" "$label"
  if (cd "$app" && "$@"); then return 0; fi
  echo "FAIL $name: $label" >&2
  return 1
}

# No `samples:` anchor trails code after cargo fmt (guides/samples.md#anchors).
anchors_alone() {
  trailing=$(grep -rnE '[^[:space:]].*(//|[{]#|#) samples: ' src templates migration tests | grep -vE ':[[:space:]]*(//|[{]#|#) samples: ' || true)
  [ -z "$trailing" ] || { printf '%s\n' "$trailing"; return 1; }
}

# Parts' tests known to be flaky under a parallel run, space-separated: one retry is allowed for
# these names only, and any other flaky test fails the run. None today; a flake found here is a
# bug, fixed where it lives (a part's in the samples repo, then synced), not listed for good.
KNOWN_FLAKY=""

# verify: `cargo offrails verify --fast` (fmt, CSS, clippy, one build, SQLite + Postgres at once),
# failing on any flaky test not in KNOWN_FLAKY (nextest retries once: NEXTEST_RETRIES).
verify() {
  if [ "$postgres" = 1 ]; then
    TEST_DATABASE_URL=$pg_url NEXTEST_RETRIES=1 cargo offrails verify --fast || return 1
  else
    NEXTEST_RETRIES=1 cargo offrails verify --fast --no-postgres || return 1
  fi
  unknown=$(cat target/verify/tests-*.log 2>/dev/null | grep -E '^[[:space:]]*FLAKY' | awk '{ print $NF }' | while IFS= read -r t; do
    case " $KNOWN_FLAKY " in (*" $t "*) ;; (*) echo "$t" ;; esac
  done)
  [ -z "$unknown" ] || {
    echo "flaky, and not a known samples flake: $unknown" >&2
    # The failed attempt's output is only in target/verify/, which CI doesn't keep: print it
    # (verify runs nextest at --status-level retry: a "TRY n FAIL … <test>" line, then its
    # indented stdout ─── / stderr ─── blocks).
    for t in $unknown; do
      echo "--- $t: the failed attempt" >&2
      cat target/verify/tests-*.log 2>/dev/null | awk -v t="$t" '
        /^ *(TRY [0-9]+ (PASS|FAIL)|PASS|FAIL|FLAKY|SIGSEGV|SIGABRT|TIMEOUT|Summary) / || /^─/ {
          show = ($0 ~ /TRY [0-9]+ FAIL/ && index($0, t) > 0)
        }
        show' | head -200 >&2
    done
    return 1
  }
}

# The boosted-navigation browser smoke (.offrails/tools/browser-smoke/smoke-boosted.mjs) against the
# app served in development on port $1 (its own SQLite file, the demo account when auth is in).
# The server is stopped by its PID. No Chrome (exit 77): a skip, recorded for the summary.
boosted_smoke() {
  port=$1
  db="sqlite://storage/browser-smoke.sqlite3?mode=rwc"
  log="target/browser-smoke-server.log"
  APP_ENV=development PORT=$port DATABASE_URL=$db cargo run -q -- db migrate >"$log" 2>&1 || { tail -20 "$log"; return 1; }
  if grep -q '"auth_demo"' src/tasks/mod.rs 2>/dev/null; then
    APP_ENV=development PORT=$port DATABASE_URL=$db cargo run -q -- task auth_demo >>"$log" 2>&1 || { tail -20 "$log"; return 1; }
  fi
  APP_ENV=development PORT=$port DATABASE_URL=$db cargo run -q -- serve >>"$log" 2>&1 &
  server=$!
  up=0
  i=0
  while [ "$i" -lt 180 ]; do
    if curl -fsS "http://localhost:$port/up" >/dev/null 2>&1; then up=1; break; fi
    kill -0 "$server" 2>/dev/null || break
    sleep 1
    i=$((i + 1))
  done
  if [ "$up" = 1 ]; then
    BASE="http://localhost:$port" node .offrails/tools/browser-smoke/smoke-boosted.mjs
    status=$?
  else
    echo "the app did not answer /up on port $port:" >&2
    tail -20 "$log" >&2
    status=1
  fi
  kill "$server" 2>/dev/null
  wait "$server" 2>/dev/null
  if [ "$status" = 77 ]; then
    echo "skip: no Chrome" > "$work/$name/browser-smoke-skipped"
    return 0
  fi
  return "$status"
}

# The Definition of Done on a generated app. $1: memory budget in MiB (none: skip that step).
# $2: a port for the boosted-navigation browser smoke (none: skip it).
gates() {
  step "anchors stand alone" anchors_alone || return 1
  step "tailwind" sh -c 'bin/setup-tailwind >/dev/null' || return 1
  step "verify (fmt, css, clippy, tests on SQLite + Postgres)" verify || return 1
  step "routes" sh -c 'APP_ENV=development cargo run -q -- routes > /dev/null' || return 1
  step "clippy (no unwrap/expect)" env CARGO_TARGET_DIR="$CARGO_TARGET_DIR/clippy" cargo clippy -q --lib --bins -- -D warnings -D clippy::unwrap_used -D clippy::expect_used || return 1
  step "offrails-check" sh .offrails/tools/offrails-check || return 1
  # A new RustSec advisory in the template's lock file otherwise reaches users first: every new
  # app's CI runs cargo deny (its .github/workflows/ci.yml), so the template's must pass too.
  if command -v cargo-deny >/dev/null 2>&1; then
    step "cargo deny (advisories, licenses, bans, sources)" cargo deny --locked check || return 1
  else
    echo "  skip: cargo deny (cargo-deny is not installed)"
  fi
  if [ "$1" != none ]; then
    step "memory budget" sh "$root/tools/ci/memory-budget.sh" . "$1" || return 1
  fi
  if [ "${2:-none}" != none ]; then
    if command -v node >/dev/null 2>&1; then
      step "browser smoke (boosted navigation)" boosted_smoke "$2" || return 1
    else
      echo "skip: no Node" > "$work/$name/browser-smoke-skipped"
    fi
  fi
}

# A feature whose criteria cite tests, so PROOF.md has something to trace (prove target).
proof_fixture() {
  mkdir -p product/features
  cat > product/features/home.md <<'SPEC'
# Feature: home

## Acceptance criteria

- [x] AC-home-1: The home page shows the app's name (`/`)
- [x] AC-home-2: The component gallery renders every component in development

## Security (agent-written threat model)

| STRIDE | Threat | Control | Test |
|--------|--------|---------|------|
| Spoofing | n/a: no accounts on these pages | — | — |
| Tampering | n/a: read-only pages | — | — |
| Repudiation | n/a: nothing to trace | — | — |
| Information disclosure | the gallery in production | mounted in development only | `the_gallery_exists_in_development_only` |
| Denial of service | n/a: static pages | — | — |
| Elevation of privilege | n/a: no roles | — | — |
SPEC
  cat > tests/app/proof_fixture_test.rs <<'TEST'
use crate::support::TestApp;

/// AC-home-1
#[tokio::test]
async fn the_home_page_shows_the_app_name() {
    TestApp::new().await.get("/").await.assert_ok();
}

/// AC-home-2
#[tokio::test]
async fn the_gallery_renders_in_development() {
    let app = TestApp::with_config(APP_CRATE::config::Config {
        env: APP_CRATE::config::AppEnv::Development,
        ..APP_CRATE::config::Config::test()
    })
    .await;
    app.get(&gallery_path()).await.assert_ok();
}

/// A helper whose comment cites AC-home-2: not a test, so PROOF.md must not list it.
fn gallery_path() -> String {
    APP_CRATE::routes::paths::dev_components()
}
TEST
  # The a11y smoke's scenario, so prove_app runs it the way `verify --full` does.
  mkdir -p tests/e2e
  cat > tests/e2e/smoke.scenario.mjs <<'SCENARIO'
// The a11y smoke's pages (test-template.sh prove).
export const a11yPaths = ["/", "/dev/components"];
SCENARIO
  # A sticky-footer layout, as apps often make it (<body> a flex column, <main> grows): pages that
  # replace the body block (/dev/components, /dev/layouts/*) must not scroll sideways in it at 390 px.
  perl -pi -e 's/<body hx-boost="true" class="min-h-screen /<body hx-boost="true" class="flex min-h-screen flex-col /; s/<main id="main" class="mx-auto max-w-3xl /<main id="main" class="mx-auto w-full max-w-3xl flex-1 /' templates/layouts/application.html
  if ! grep -q 'class="flex min-h-screen flex-col ' templates/layouts/application.html || ! grep -q 'w-full max-w-3xl flex-1' templates/layouts/application.html; then
    echo "proof_fixture: the layout edit did not apply" >&2
    return 1
  fi
  krate=$(awk -F'"' '/^name = /{ print $2; exit }' Cargo.toml)
  perl -pi -e "s/APP_CRATE/$krate/g" tests/app/proof_fixture_test.rs
  printf 'mod proof_fixture_test;\n' >> tests/app/main.rs
  cargo fmt --all
}

# prove_app <port>: serve the app in development (the demo account when auth is in), run the
# component smoke and `cargo offrails prove` twice, check PROOF.md, stop the server by its PID.
prove_app() {
  port=$1
  db="sqlite://storage/prove.sqlite3?mode=rwc"
  log="target/prove-server.log"
  APP_ENV=development PORT=$port DATABASE_URL=$db cargo run -q -- db migrate >"$log" 2>&1 || { tail -20 "$log"; return 1; }
  if grep -q '"auth_demo"' src/tasks/mod.rs 2>/dev/null; then
    APP_ENV=development PORT=$port DATABASE_URL=$db cargo run -q -- task auth_demo >>"$log" 2>&1 || { tail -20 "$log"; return 1; }
  fi
  APP_ENV=development PORT=$port DATABASE_URL=$db cargo run -q -- serve >>"$log" 2>&1 &
  server=$!
  up=0
  i=0
  while [ "$i" -lt 180 ]; do
    if curl -fsS "http://localhost:$port/up" >/dev/null 2>&1; then up=1; break; fi
    kill -0 "$server" 2>/dev/null || break
    sleep 1
    i=$((i + 1))
  done
  status=0
  if [ "$up" != 1 ]; then
    echo "the app did not answer /up on port $port:" >&2
    tail -20 "$log" >&2
    status=1
  fi
  base="http://localhost:$port"
  if [ "$status" = 0 ]; then
    BASE=$base node .offrails/tools/browser-smoke/smoke-components.mjs || status=$?
  fi
  # The a11y smoke exactly as `verify --full` runs it (the vendored run.sh, the scenario, exit 77 a
  # skip): run.sh runs a copy of lib.mjs from its own directory under the cache, so axe-core must be
  # found from there.
  if [ "$status" = 0 ]; then
    a11y=0
    BASE=$base SCENARIO=tests/e2e/smoke.scenario.mjs sh .offrails/tools/browser-smoke/run.sh a11y || a11y=$?
    if [ "$a11y" = 77 ]; then
      echo "skip: a11y smoke (run.sh exit 77)" > "$work/$name/browser-smoke-skipped"
    elif [ "$a11y" != 0 ]; then
      status=$a11y
    fi
  fi
  if [ "$status" = 0 ]; then
    t0=$(date +%s)
    BASE=$base cargo offrails prove || status=$?
    t1=$(date +%s)
  fi
  if [ "$status" = 0 ]; then
    BASE=$base cargo offrails prove > target/prove-second.log 2>&1 || status=$?
    t2=$(date +%s)
    tail -3 target/prove-second.log
    echo "prove timings: first run $((t1 - t0)) s, second run $((t2 - t1)) s" | tee "$work/$name/prove-timings-$port.txt"
  fi
  kill "$server" 2>/dev/null
  wait "$server" 2>/dev/null
  if [ "$status" = 77 ]; then
    echo "skip: no Chrome or Node" > "$work/$name/browser-smoke-skipped"
    return 0
  fi
  [ "$status" = 0 ] || return "$status"
  proof=docs/proof/PROOF.md
  grep -q '^\*\*Result: PROVEN\.\*\*' "$proof" || { cat "$proof"; return 1; }
  pages=$(grep -c '^| `/' "$proof")
  shots=$(find docs/proof/screens -name '*.png' | wc -l | tr -d ' ')
  [ "$shots" -eq $((pages * 4)) ] || { echo "PROOF.md lists $pages pages but there are $shots screenshots" >&2; return 1; }
  # shellcheck disable=SC2016 # literal backticks
  grep -q 'AC-home-1.*the_home_page_shows_the_app_name.*SQLite: passed.*`/` ok' "$proof" || { echo "AC-home-1 is not traced to its passing test and page" >&2; cat "$proof"; return 1; }
  grep -q 'screens/dev-components--mobile-dark.png' "$proof" || return 1
  grep -q 'AC-home-2.*the_gallery_renders_in_development' "$proof" || { echo "AC-home-2 is not traced to its test" >&2; return 1; }
  ! grep -q 'gallery_path' "$proof" || { echo "PROOF.md lists a helper that is not a test as citing an AC" >&2; grep -n gallery_path "$proof" >&2; return 1; }
  # The gallery and the layout pages were checked at 390 px (in the sticky-footer layout) and passed.
  for p in /dev/components /dev/layouts/sidebar /dev/layouts/marketing /dev/layouts/auth /dev/layouts/docs; do
    grep -q "^| \`$p\` | [^|]* | ok | " "$proof" || { echo "PROOF.md: $p is not ok" >&2; grep -F "$p" "$proof" >&2; return 1; }
  done
  grep -q '| no |' "$proof" || { echo "the second run compared nothing with the first" >&2; return 1; }
  git check-ignore -q docs/proof/screens/home--desktop-light.png || { echo "screenshots are not git-ignored" >&2; return 1; }
  cp "$proof" "$work/$name/PROOF-$port.md"
}

commit_all() {
  # shellcheck disable=SC2016 # $0 is the inner shell's
  step "commit: $1" sh -c 'git add -A && git -c user.name=ci -c user.email=ci@example.invalid commit -qm "$0" --allow-empty' "$1"
}

# generate_all <file>: one `cargo offrails generate` per line of <file>, then each again (a no-op).
generate_all() {
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    # shellcheck disable=SC2086 # one generator's words
    step "generate $line" cargo offrails generate $line || return 1
  done < "$1"
  commit_all "generated" || return 1
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    # shellcheck disable=SC2086
    step "generate $line again (no-op)" sh -c 'cargo offrails generate "$@" | grep -q "nothing to do"' sh $line || return 1
  done < "$1"
  # shellcheck disable=SC2016 # expanded by the inner shell
  step "re-running generators changed nothing" sh -c 'test -z "$(git status --porcelain)" || { git status --short; exit 1; }'
}

# destroy_all <file>: `cargo offrails destroy <kind> <name>` for each line, last first; then the app
# must be exactly the baseline commit.
destroy_all() {
  lines=$(awk 'NF { print }' "$1" | sed '1!G;h;$!d')
  printf '%s\n' "$lines" | while IFS= read -r line; do
    # shellcheck disable=SC2086 # one generator's words
    set -- $line
    kind=$1
    if [ "$kind" = test ]; then gname="$2 $3"; else gname=$2; fi
    # shellcheck disable=SC2086
    step "destroy $kind $gname" cargo offrails destroy $kind $gname || exit 1
  done || return 1
  # shellcheck disable=SC2016 # expanded by the inner shell
  step "destroy left the app as it was" sh -c 'test -z "$(git diff --name-only baseline)" && test -z "$(git ls-files --others --exclude-standard)" || { git diff --stat baseline; git ls-files --others --exclude-standard; exit 1; }'
}

# The snippets, dependencies first; one line per `snippet add` (its words after `add`).
SNIPPETS="resource
resource --as article
slug
public-show-by-slug
atom-feed
markdown
pagination
theme-toggle
seed-backdated"

# snippets_round <app dir name> <new options…>: add every snippet, the gates, re-add (no-op),
# remove in reverse, and the app is the baseline again.
snippets_round() {
  fresh "$1"
  dir=$1
  shift
  (cd "$work/$name" && gen new "$dir" "$dir$tag" "Snippet App" "$@") || return 1
  # The crates snippets add stay when they're removed (like a generator's): add them first.
  step "crates the snippets add" sh -c 'cargo add -q pulldown-cmark ammonia' || return 1
  commit_all "baseline" && step "tag baseline" git tag baseline || return 1
  printf '%s\n' "$SNIPPETS" > "$work/$name/$dir.snippets"
  while IFS= read -r line; do
    # shellcheck disable=SC2086 # one snippet's words
    step "snippet add $line" cargo offrails snippet add $line || return 1
  done < "$work/$name/$dir.snippets"
  commit_all "snippets" || return 1
  while IFS= read -r line; do
    # shellcheck disable=SC2086
    step "snippet add $line again (no-op)" sh -c 'cargo offrails snippet add "$@" | grep -q "nothing to do"' sh $line || return 1
  done < "$work/$name/$dir.snippets"
  gates none || return 1
  lines=$(sed '1!G;h;$!d' "$work/$name/$dir.snippets")
  printf '%s\n' "$lines" | while IFS= read -r line; do
    # shellcheck disable=SC2086
    step "snippet remove $line" cargo offrails snippet remove $line || exit 1
  done || return 1
  # shellcheck disable=SC2016 # expanded by the inner shell
  step "removing left the app as it was" sh -c 'test -z "$(git diff --name-only baseline)" && test -z "$(git ls-files --others --exclude-standard)" || { git diff --stat baseline; git ls-files --others --exclude-standard; exit 1; }'
}

fresh() {
  app="$work/$name/$1"
  rm -rf "${work:?}/${name:?}"
  mkdir -p "$work/$name"
}

# fresh without clearing the target's other apps (a target that builds two).
fresh_keep() {
  app="$work/$name/$1"
  rm -rf "${work:?}/${name:?}/${1:?}"
  mkdir -p "$work/$name"
}

run_one() {
  name=$1
  case "$name" in
    gen)
      app="$root/tools/offrails-gen"
      step "unit tests" cargo test -q --target-dir "$work/gen-target" || return 1
      step "clippy" cargo clippy -q --all-targets --target-dir "$work/gen-target" -- -D warnings || return 1
      step "fmt --check" cargo fmt --check || return 1
      ;;
    bare)
      fresh bare_app
      (cd "$work/$name" && gen new bare_app "bare_app$tag" "Bare App") || return 1
      gates 24 "$smoke_port" || return 1
      ;;
    all)
      fresh all_app
      (cd "$work/$name" && gen new all_app "all_app$tag" "All Parts" --parts all) || return 1
      gates 32 "$((smoke_port + 1))" || return 1
      ;;
    incremental)
      fresh inc_app
      (cd "$work/$name" && gen new inc_app "inc_app$tag" "Incremental App") || return 1
      step "baseline commit" sh -c 'git add -A && git -c user.name=ci -c user.email=ci@example.invalid commit -qm baseline' || return 1
      parts=$(gen list-parts | awk 'NR > 1 && NF > 2 && $1 !~ /^Add|^Which/ { print $1 }')
      for p in $parts; do
        step "add-part $p" cargo offrails add-part "$p" || return 1
      done
      step "commit parts" sh -c 'git add -A && git -c user.name=ci -c user.email=ci@example.invalid commit -qm parts' || return 1
      for p in $parts; do
        step "add-part $p again (no-op)" cargo offrails add-part "$p" || return 1
      done
      # shellcheck disable=SC2016 # expanded by the inner shell
      step "re-running add-part changed nothing" sh -c 'test -z "$(git status --porcelain)" || { git status --short; exit 1; }' || return 1
      step "list-parts shows them installed" sh -c '! cargo offrails list-parts | grep -q "^available"' || return 1
      gates 32 || return 1
      ;;
    generators)
      # Bare: no accounts, so every resource is --public. Crates the generators add are added
      # first, so the destroyed app matches the baseline byte for byte.
      fresh gen_bare
      (cd "$work/$name" && gen new gen_bare "gen_bare$tag" "Generated Bare") || return 1
      step "crates the generators add" sh -c 'cargo add -q rust_decimal && cargo add -q sea-orm --features with-rust_decimal' || return 1
      commit_all "baseline" && step "tag baseline" git tag baseline || return 1
      cat > "$work/$name/bare.gens" <<'GENS'
resource authors name:string --public
resource books title:string isbn:string:unique author:references --public
resource chapters title:string number:integer book:references --public
resource items name:string notes:text:optional count:integer big:bigint flag:boolean active:boolean:optional weight:decimal{8.3} price:money cost:money:optional at:datetime day:date:optional code:uuid:unique meta:json:optional book:references:optional --public
page about --nav
page profile --path /people/{handle}
controller dashboard index summary
controller blog index show:/blog/{slug}
migration add_subtitle_to_books subtitle:string:optional:index
migration backfill_isbns
test unit isbn_checksum
GENS
      generate_all "$work/$name/bare.gens" || return 1
      gates none || return 1
      destroy_all "$work/$name/bare.gens" || return 1
      # Every part: accounts, so resources are scoped to their owner (and the tenancy suites).
      fresh gen_all
      (cd "$work/$name" && gen new gen_all "gen_all$tag" "Generated All" --parts all) || return 1
      step "crates the generators add" sh -c 'cargo add -q rust_decimal && cargo add -q sea-orm --features with-rust_decimal && cargo add -q utoipa --features axum_extras,chrono' || return 1
      commit_all "baseline" && step "tag baseline" git tag baseline || return 1
      cat > "$work/$name/all.gens" <<'GENS'
resource posts title:string slug:string:unique body:text published_at:datetime:optional
resource comments body:text post:references
resource items name:string notes:text:optional count:integer big:bigint flag:boolean active:boolean:optional weight:decimal{8.3} price:money cost:money:optional at:datetime day:date:optional code:uuid:unique meta:json:optional post:references:optional
model tag name:string:unique
api notes title:string body:text done:boolean
api posts
job notify_post_published post_id:bigint --queue mailers
mailer post_mailer published digest
channel post
page about
controller reports index
controller archive month:/archive/{year}/{month_id}
migration add_pinned_to_posts pinned:boolean
test request checkout
GENS
      generate_all "$work/$name/all.gens" || return 1
      gates none || return 1
      destroy_all "$work/$name/all.gens" || return 1
      ;;
    snippets)
      for toml in "$root"/template/snippets/*/snippet.toml; do
        sn=$(basename "$(dirname "$toml")")
        printf '%s\n' "$SNIPPETS" | grep -q "^$sn\( \|$\)" || { echo "test-template: snippet $sn is not in SNIPPETS" >&2; return 1; }
      done
      snippets_round snip_bare || return 1
      snippets_round snip_all --parts all || return 1
      ;;
    prove)
      if command -v node >/dev/null 2>&1; then
        app="$root"
        step "prove's pixel diff (selftest)" node tools/browser-smoke/selftest-prove.mjs || return 1
      fi
      for kind in bare all; do
        if [ "$kind" = bare ]; then
          fresh_keep prove_bare
          (cd "$work/$name" && gen new prove_bare "prove_bare$tag" "Prove Bare") || return 1
          port=$((smoke_port + 2))
        else
          fresh_keep prove_all
          (cd "$work/$name" && gen new prove_all "prove_all$tag" "Prove All" --parts all) || return 1
          port=$((smoke_port + 3))
        fi
        step "fixture feature" proof_fixture || return 1
        step "tailwind" sh -c 'bin/setup-tailwind >/dev/null' || return 1
        step "verify (the suites PROOF.md reports)" verify || return 1
        step "offrails-check" sh .offrails/tools/offrails-check || return 1
        if command -v node >/dev/null 2>&1; then
          step "prove (screenshots, checks, PROOF.md; twice)" prove_app "$port" || return 1
        else
          echo "skip: no Node" > "$work/$name/browser-smoke-skipped"
        fi
      done
      ;;
    update-part)
      fresh up_app
      (cd "$work/$name" && gen new up_app "up_app$tag" "Update App" --parts mailer) || return 1
      commit_all "baseline" || return 1
      # A newer framework whose mailer changed one line of a file and one wire.
      newer="$work/$name/newer-fw"
      mkdir -p "$newer" && cp -R "$root/VERSION" "$root/template" "$newer/" || return 1
      perl -pi -e 's/^\/\/! `TestMailer`: one harmless/\/\/! `TestMailer` (newer): one harmless/' \
        "$newer/template/parts/mailer/files/src/mailers/test_mailer.rs" || return 1
      perl -pi -e 's/^mod mailer_test;$/mod mailer_test; \/\/ the newer wire/; s/^version = "(.*)"$/version = "$1-newer"/' \
        "$newer/template/parts/mailer/sample.toml" || return 1
      # Clean: the app changed another line of the same file.
      step "app edit elsewhere" perl -pi -e 's/Test email from \{app_name\}/Test email (app) from {app_name}/' src/mailers/test_mailer.rs || return 1
      step "update-part merges cleanly" cargo offrails update-part mailer --framework "$newer" || return 1
      step "both changes are in, no markers" sh -c 'grep -q "(newer): one harmless" src/mailers/test_mailer.rs && grep -q "Test email (app) from" src/mailers/test_mailer.rs && grep -q "mod mailer_test; // the newer wire" tests/app/main.rs && ! grep -rq "<<<<<<<" src tests' || return 1
      step "clippy after the merge" cargo clippy -q --all-targets -- -D warnings || return 1
      # Conflict: the app changed the same line.
      step "reset" sh -c 'git checkout -q . && git clean -qfd' || return 1
      # shellcheck disable=SC2016 # literal backticks in a perl pattern, not an expansion
      step "app edit on the same line" perl -pi -e 's/^\/\/! `TestMailer`: one harmless/\/\/! `TestMailer` (app): one harmless/' src/mailers/test_mailer.rs || return 1
      # shellcheck disable=SC2016 # $0 expands in the inner sh, by design
      step "update-part reports the conflict (exit 1)" sh -c '! cargo offrails update-part mailer --framework "$0"' "$newer" || return 1
      step "markers hold both sides" sh -c 'grep -q "^<<<<<<< app" src/mailers/test_mailer.rs && grep -q "(app): one harmless" src/mailers/test_mailer.rs && grep -q "(newer): one harmless" src/mailers/test_mailer.rs && grep -q "^>>>>>>> mailer" src/mailers/test_mailer.rs' || return 1
      ;;
    *) echo "test-template: unknown target $name" >&2; return 1 ;;
  esac
}

summary=""
failed=0
for t in $targets; do
  if run_one "$t"; then result="ok  "; else result="FAIL"; failed=$((failed + 1)); fi
  if [ "$postgres" = 1 ]; then dbs="SQLite + Postgres"; else dbs="SQLite only (Postgres skipped)"; fi
  [ "$t" = gen ] && dbs="no database"
  [ "$t" = update-part ] && dbs="no tests run"
  [ "$t" = snippets ] && dbs="$dbs, bare and every part"
  if [ -f "$work/$t/browser-smoke-skipped" ]; then dbs="$dbs; browser smoke SKIPPED: $(cat "$work/$t/browser-smoke-skipped")"; fi
  summary="$summary
$result $t ($dbs)"
done
printf '\n== summary (apps in %s)%s\n' "$work" "$summary"
[ "$failed" = 0 ]
