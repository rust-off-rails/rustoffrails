#!/usr/bin/env bash
# Regression tests for the plugin hooks: each case feeds a hook the JSON Claude Code sends
# and asserts the exit code (2 = blocked/flagged, 0 = allowed).
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"; H="$ROOT/hooks"
APP="$(mktemp -d)"; trap 'rm -rf "$APP"' EXIT
mkdir -p "$APP/.offrails" "$APP/src/models" "$APP/src/kernel" "$APP/templates/books" "$APP/web/src" "$APP/tests/app" "$APP/migration/src"
fail=0
check() { # <expected exit> <hook> <json> <label>
  printf '%s' "$3" | CLAUDE_PROJECT_DIR="$APP" bash "$H/$2" >/dev/null 2>&1; got=$?
  if [ "$got" = "$1" ]; then echo "ok   $4"; else echo "FAIL $4 (expected $1, got $got)"; fail=1; fi
}
j() { printf '{"tool_input":{"file_path":"%s"}}' "$APP/$1"; }

check 2 guard-edits.sh "$(j .offrails/stack.md)"                 "edit to .offrails/ is blocked"
check 2 guard-edits.sh "$(j web/src/types/Book.ts)"              "edit to generated TS types is blocked"
check 0 guard-edits.sh "$(j src/models/book.rs)"                 "edit to app code is allowed"
(cd "$APP" && git init -q && echo a > migration/src/m20260101_000000_create_x.rs && git add -A && git -c user.email=t@t -c user.name=t commit -qm x)
check 2 guard-edits.sh "$(j migration/src/m20260101_000000_create_x.rs)" "edit to a committed migration is blocked"
ep() { printf '{"tool_input":{"file_path":"%s"}}' "$1"; }
check 2 guard-edits.sh "$(ep "$APP/./.offrails/x")"                "dot segment before .offrails/ is blocked"
check 2 guard-edits.sh "$(ep "$APP/src/../.offrails/x")"           ".. segment into .offrails/ is blocked"
check 2 guard-edits.sh "$(ep "$APP/src/../migration/src/m20260101_000000_create_x.rs")" ".. path to a committed migration is blocked"
check 0 guard-edits.sh "$(ep "$APP/src/../src/models/book.rs")"    ".. path to app code is allowed"
printf '{"cwd":"%s","tool_input":{"file_path":"./.offrails/x"}}' "$APP" | CLAUDE_PROJECT_DIR="$APP" bash "$H/guard-edits.sh" >/dev/null 2>&1; [ $? = 2 ] && echo "ok   relative ./.offrails path is blocked" || { echo "FAIL relative ./.offrails path"; fail=1; }
printf '{"cwd":"%s","tool_input":{"file_path":"src/models/book.rs"}}' "$APP" | CLAUDE_PROJECT_DIR="$APP" bash "$H/guard-edits.sh" >/dev/null 2>&1; [ $? = 0 ] && echo "ok   relative app path is allowed" || { echo "FAIL relative app path"; fail=1; }
LINK="$(mktemp -d)/link"; ln -s "$APP" "$LINK"
printf '%s' "$(ep "$LINK/.offrails/x")" | CLAUDE_PROJECT_DIR="$APP" bash "$H/guard-edits.sh" >/dev/null 2>&1; [ $? = 2 ] && echo "ok   symlinked project dir path is blocked" || { echo "FAIL symlinked project dir"; fail=1; }
printf '%s' "$(ep "$APP/.offrails/x")" | CLAUDE_PROJECT_DIR="$LINK" bash "$H/guard-edits.sh" >/dev/null 2>&1; [ $? = 2 ] && echo "ok   symlinked CLAUDE_PROJECT_DIR is blocked" || { echo "FAIL symlinked CLAUDE_PROJECT_DIR"; fail=1; }
rm -rf "$(dirname "$LINK")"
check 2 guard-edits.sh "$(printf '{"tool_input":{"notebook_path":"%s"}}' "$APP/.offrails/n.ipynb")" "NotebookEdit into .offrails/ is blocked"
check 0 guard-edits.sh "$(printf '{"tool_input":{"notebook_path":"%s"}}' "$APP/n.ipynb")" "NotebookEdit of app notebook is allowed"
check 0 guard-edits.sh "$(j migration/src/m20260102_000000_add_y.rs)"    "edit to a new migration is allowed"

printf 'db.execute_unprepared("x");\n' > "$APP/src/models/a.rs";              check 2 check-conventions.sh "$(j src/models/a.rs)" "raw SQL outside kernel is flagged"
printf 'db.execute_unprepared("x");\n' > "$APP/src/kernel/db.rs";             check 0 check-conventions.sh "$(j src/kernel/db.rs)" "raw SQL inside kernel is allowed"
printf 'let t = db.begin().await?;\n' > "$APP/src/models/b.rs";               check 2 check-conventions.sh "$(j src/models/b.rs)" "raw .begin() is flagged"
printf '.route("/books/:id", get(show))\n' > "$APP/src/models/c.rs";          check 2 check-conventions.sh "$(j src/models/c.rs)" "/:id route is flagged"
printf 'let q = format!("SELECT * FROM t WHERE x = {x}");\n' > "$APP/src/kernel/q.rs"; check 2 check-conventions.sh "$(j src/kernel/q.rs)" "format! SQL is flagged even in kernel"
printf 'CorsLayer::permissive()\n' > "$APP/src/models/d.rs";                  check 2 check-conventions.sh "$(j src/models/d.rs)" "permissive CORS is flagged"
printf "\"script-src 'self' 'unsafe-inline'\"\n" > "$APP/src/kernel/h.rs";    check 2 check-conventions.sh "$(j src/kernel/h.rs)" "unsafe-inline CSP is flagged"
printf '<p>{{ body|safe }}</p>\n' > "$APP/templates/books/a.html";            check 2 check-conventions.sh "$(j templates/books/a.html)" "Askama |safe is flagged"
printf '<button onclick="x()">x</button>\n' > "$APP/templates/books/b.html";  check 2 check-conventions.sh "$(j templates/books/b.html)" "inline on* handler is flagged"
printf '<p>{{ book.title }}</p>\n' > "$APP/templates/books/ok.html";          check 0 check-conventions.sh "$(j templates/books/ok.html)" "a clean template passes"
printf 'x = <div dangerouslySetInnerHTML={{__html: h}} />\n' > "$APP/web/src/A.tsx"; check 2 check-conventions.sh "$(j web/src/A.tsx)" "dangerouslySetInnerHTML is flagged"
printf '{@html body}\n' > "$APP/web/src/A.svelte";                            check 2 check-conventions.sh "$(j web/src/A.svelte)" "Svelte {@html} is flagged"
printf 'tokio::time::sleep(d).await;\n' > "$APP/tests/app/x_test.rs";         check 2 check-conventions.sh "$(j tests/app/x_test.rs)" "sleep in tests is flagged"

printf '[package]\nname = "x"\n[dependencies]\nonce_cell = "1"\n' > "$APP/Cargo.toml";   check 2 check-conventions.sh "$(j Cargo.toml)" "banned crate in Cargo.toml is flagged"
printf '[package]\nname = "x"\n[dependencies]\naxum = "0.8"\n' > "$APP/Cargo.toml";     check 0 check-conventions.sh "$(j Cargo.toml)" "blessed crate in Cargo.toml passes"

b() { printf '{"tool_input":{"command":"%s"}}' "$1"; }
check 2 guard-bash.sh "$(b 'pkill -f myapp')"                      "pkill is blocked"
check 2 guard-bash.sh "$(b 'cd x && killall myapp')"               "killall is blocked"
check 2 guard-bash.sh "$(b '/usr/bin/pkill x')"                    "absolute-path pkill is blocked"
check 2 guard-bash.sh "$(b "bash -c 'pkill x'")"                   "pkill inside bash -c is blocked"
check 2 guard-bash.sh "$(b '(pkill x)')"                           "pkill in a subshell is blocked"
check 2 guard-bash.sh "$(b 'p\\\\kill x')"                          "backslash-escaped pkill is blocked"
check 2 guard-bash.sh "$(b 'kill -9 $(pgrep -f app)')"             "kill of \$(pgrep) is blocked"
check 2 guard-bash.sh "$(b 'ps aux | xargs kill')"                 "ps | xargs kill is blocked"
check 2 guard-bash.sh "$(b 'pgrep -f app | xargs kill -9')"        "pgrep | xargs kill is blocked"
check 0 guard-bash.sh "$(b 'echo skillset; cargo run --bin killer')" "words merely containing kill are allowed"
check 0 guard-bash.sh "$(b 'lsof -ti tcp:3000 -sTCP:LISTEN | xargs kill')" "kill by listener port is allowed"
check 0 guard-bash.sh "$(b 'kill $(cat storage/pids/dev-3000.pid)')" "kill by PID file is allowed"

# Without jq the guards fail closed (sed parsing could be fooled by , } or " in the command).
NOJQ="$(mktemp -d)"; for t in bash cat dirname tr sed head grep git; do ln -s "$(command -v $t)" "$NOJQ/$t"; done
printf '{"tool_input":{"command":"echo a, pkill x"}}' | PATH="$NOJQ" CLAUDE_PROJECT_DIR="$APP" "$(command -v bash)" "$H/guard-bash.sh" >/dev/null 2>&1; [ $? = 2 ] && echo "ok   guard-bash fails closed without jq" || { echo "FAIL guard-bash without jq"; fail=1; }
printf '%s' "$(j .offrails/x)" | PATH="$NOJQ" CLAUDE_PROJECT_DIR="$APP" "$(command -v bash)" "$H/guard-edits.sh" >/dev/null 2>&1; [ $? = 2 ] && echo "ok   guard-edits fails closed without jq" || { echo "FAIL guard-edits without jq"; fail=1; }
rm -rf "$NOJQ"

check 0 stop-gate.sh '{"stop_hook_active":true}'                   "stop gate never loops"
out=$(printf '{}' | CLAUDE_PROJECT_DIR="$APP" bash "$H/session-start.sh"); case "$out" in *"Rust off Rails app"*) echo "ok   session start orients the agent";; *) echo "FAIL session start"; fail=1;; esac
outside=$(mktemp -d); printf '{"tool_input":{"command":"pkill x"}}' | CLAUDE_PROJECT_DIR="$outside" bash "$H/guard-bash.sh" >/dev/null 2>&1 && echo "ok   hooks are no-ops outside Off Rails apps" || { echo "FAIL hooks outside apps"; fail=1; }; rm -rf "$outside"
# Drift is by content: the installer's .sha256 and the vendored VERSION line don't count (#15).
FW1="$(mktemp -d)"; mkdir -p "$FW1/guides"; echo doc > "$FW1/guides/a.md"; echo 0.7.5 > "$FW1/VERSION"
fp1=$(bash "$H/fingerprint.sh" "$FW1"); echo 3f9c > "$FW1/.sha256"; printf '0.7.5\nvendored 2026-11-22\n' > "$FW1/VERSION"; fp2=$(bash "$H/fingerprint.sh" "$FW1")
echo changed > "$FW1/guides/a.md"; fp3=$(bash "$H/fingerprint.sh" "$FW1"); rm -rf "$FW1"
[ "$fp1" = "$fp2" ] && echo "ok   fingerprint ignores .sha256 and VERSION" || { echo "FAIL fingerprint counts .sha256 or VERSION"; fail=1; }
[ "$fp1" != "$fp3" ] && echo "ok   fingerprint sees a changed doc" || { echo "FAIL fingerprint missed a changed doc"; fail=1; }
exit $fail
