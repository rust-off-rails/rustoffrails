# Generator: `upgrade` (`rails app:update`)

> **TL;DR**
> - Brings an app to a newer **release**: `$FW` is `~/.offrails/current`, the plugin root, or a tag export, never a moving working tree.
> - Detect drift by content fingerprint; start from a clean git tree; read the CHANGELOG's App migration steps between versions.
> - Reconcile, don't re-create: classify each change (new / conforming / divergent / conflicting); never edit applied migrations; show the plan and wait for a yes.
> - Re-vendor, apply in green vertical slices, verify nothing shows a stale version, and log the upgrade in `docs/decisions.md`.
> - From 0.7.1: **compare the app with `.offrails/template/app/`** (the tested kernel of the new release) as a reference for kernel changes, never a blind overwrite; and **offer the parts** for areas the app built by hand (adopt with `cargo offrails add-part`, borrow, or skip; nothing without the human's yes).
>
> Read it all whenever you upgrade an app (`offrails upgrade` launches this recipe).

Brings an existing app up to a newer Rust off Rails release: it re-vendors `.offrails/` and migrates the app to any changed conventions. Any agent can run this recipe (Claude Code, Codex, opencode, Cursor…). In Claude Code, `/offrails:upgrade` runs it for you.

**`$FW`** is the new framework:
- after the one-line install: `~/.offrails/current` (run `offrails update` first to get the latest release);
- in Claude Code: the plugin root;
- otherwise: a checkout of the framework at a release tag. **Never vendor a moving working tree:** `vendor` and `update-part` read a checkout's HEAD (`git archive HEAD`), not its working tree, and `vendor` records the commit as `.offrails/VERSION`'s third line, `from git v<version> (<sha>)` at the tag, or `… (unreleased: …)` when HEAD is past it (a release branch before tagging, whose `VERSION` still names the previous release). Unreleased commits in a versioned app are then visible, not silent; check out the tag for a real upgrade.

Run it from the app's root (the directory with `.offrails/`).

## 1. Is there anything to upgrade?

Compare content, not just version numbers: docs can change without a bump.

```sh
bash "$FW/hooks/fingerprint.sh" .offrails
bash "$FW/hooks/fingerprint.sh" "$FW"
head -n1 .offrails/VERSION "$FW/VERSION"
```

**An installed `offrails` older than 0.6.0 silently ignores `--fw`** (and every other flag): `offrails upgrade --check --fw "$FW"` then compares against `~/.offrails/current` instead and can report "current" for the wrong release. Run the target release's own script, `sh "$FW/tools/offrails" upgrade --check --fw "$FW"`, or `offrails update` first (`offrails version` prints the installed release). From 0.6.1 the CLI rejects unknown flags with exit 2 and answers `-h`/`--help` on every subcommand without doing anything else.

If the fingerprints match, the app is current: say so and stop. (`offrails upgrade --check` does exactly this against `~/.offrails/current`; `offrails upgrade --check --fw "$FW"` against any other release, e.g. a tag export: `git -C <framework checkout> archive v0.6.0 | tar -x -C /tmp/offrails-0.6.0`. Exit 0 = current, 10 = an upgrade is due. `--check` never starts an agent; plain `offrails upgrade` from a script or another agent needs `--yes`, else it prints the agent command and exits 3.)

## 2. Start clean

Require a clean git tree. If it isn't clean, ask the human to commit first, or commit for them if they agree.

**Preconditions for the proofs.** The App migration steps prove UI changes with the framework's browser smokes (`.offrails/tools/browser-smoke/run.sh`), which need **Node ≥ 22.12** (Node 24 LTS recommended; puppeteer-core 25 refuses older) and an installed Chrome. Check `node --version` now. Too old or missing: say so in the plan, and run the smokes once a Node that qualifies is on `PATH` (`run.sh` refuses older with that message). Postgres for `TEST_DATABASE_URL` is the other precondition ([workflow](../workflow.md#definition-of-done)).

## 3. Read what changed

Read `$FW/CHANGELOG.md` from the app's version up to the new one. Each release lists **App migration** steps written for an agent, in reconcile style (detect → final shape → converge). Then diff the docs (`diff -ru .offrails "$FW" -x .git -x .claude-plugin -x skills -x hooks -x evals -x brand -x .in_use`) to catch anything the changelog doesn't mention.

**Compare with the template (0.7.1 and later).** `$FW/template/app/` is the new release's kernel, built and tested. Two diffs show what changed and where the app stands:

```sh
diff -ru .offrails/template/app "$FW/template/app"     # what the release changed in the kernel (old template → new); empty before 0.7.1
diff -ru -x target -x .git -x .offrails -x storage "$FW/template/app" .   # the app against the new template: long, the app has grown
```

Read the first one change by change: each is a candidate slice for step 7 (a fixed header, a new test, a hardened default). Use the second only to locate the app's version of each changed file. **It's a reference, never an overwrite:** the app's files have diverged on purpose (its routes, models, layout, copy, renamed crate), so never copy a template file over an app file. Port each change into the app's code, with its test, and classify it like any other change (step 4). Template tokens (`offrails_app`, `Offrails App`) are the app's crate and product name in the app.

## 4. Reconcile, don't re-create

For every change, first look for an equivalent the app already has, often built early from older docs or by hand: a table, migration, kernel module, route, template or test. Classify each change:

- **new:** absent. Build it per the new docs.
- **present, conforming:** matches the new spec. Nothing to do, though you may add missing tests.
- **present, divergent:** exists in another shape, so reconcile it in place. **Never** re-create or edit an applied migration. Write a new migration that alters, renames, backfills or copies data, keeps existing rows, and has a working `down`. Rename modules, routes and templates to the spec's names, with redirects for any URL that moved.
- **conflicting:** the app's version encodes a product decision (check `docs/decisions.md` and `product/`). Keep it, record it as a deviation, and ask the human only if it blocks the upgrade.

## 5. Show the plan, wait for a yes

List each change with its classification and what it means for this app: files affected, migrations, risk. Upgrading to 0.7.0 or later, add the parts offer from [step 7b](#7b-offer-the-parts) to the same message, so the human answers once. Wait for the human's yes.

Non-interactive runs (a headless agent, or a request relayed by another agent): proceed only if the human's request explicitly asked for the upgrade and nothing in the plan is outward-facing (no deploys, no production data). Write the plan to `docs/decisions.md` first, so the human can review what was decided.

## 6. Update the parts, then re-vendor the framework

**First, while `.offrails/` is still the old release** (it is the base of the merge), bring each installed part to the new version:

```sh
cargo offrails update-part <part>… --framework "$FW" --dry-run   # the plan: take / keep / merge / CONFLICT per file and wire
cargo offrails update-part <part>… --framework "$FW"
```

**An app on 0.7.1 or older has no `update-part`** (its `cargo offrails` is the vendored, older offrails-gen): run the new release's instead, the same arguments plus `--app .`:

```sh
cargo run -q --manifest-path "$FW/tools/offrails-gen/Cargo.toml" --target-dir "target/offrails-gen-$(cat "$FW/VERSION")" -- \
  update-part <part>… --framework "$FW" --app . --dry-run    # then without --dry-run
```

After step 6's re-vendoring, `cargo offrails` is the new release's, and `update-part` works directly from then on.

Per file it is a three-way merge: the part as installed (`.offrails/`), the new part (`$FW`), the app's copy. A file only the part changed is taken; one only the app changed is kept; both changed merges line by line, and overlapping edits are written with `<<<<<<< app` / `||||||| <part> <old>` / `=======` / `>>>>>>> <part> <new>` markers (exit 1). Wiring the part changed is swapped where the old text still stands. It never overwrites an app change silently. Resolve every marker (`grep -rn '<<<<<<<' src templates tests migration`), then `cargo fmt --all`, `cargo run -q -- db migrate`, and the part's CHECKS.md. `update-part` logs `updated sample <name> <old> -> <new>` in `docs/decisions.md`.


Replace `.offrails/` with the release, with the same exclusions as [`new.md` step 1](new.md#1-generate), and write `.offrails/VERSION` (framework version, then `vendored <date>`). On any OS:

```sh
cargo run -q --manifest-path "$FW/tools/offrails-gen/Cargo.toml" --target-dir "target/offrails-gen-$(cat "$FW/VERSION")" -- vendor --framework "$FW"
```

The target directory is the release's own (`target/offrails-gen-0.7.3`), never `target/offrails-gen`: a release whose files are older than the binary cached there would reuse the previous release's offrails-gen (seen as `update-part`: "unknown command"). Delete the old ones when you're done (`rm -rf target/offrails-gen-*`).

(Or `rsync -a --delete --exclude /.git --exclude /.claude-plugin --exclude /skills --exclude /hooks --exclude /evals --exclude /brand --exclude /.in_use --exclude /media --exclude /.sha256 "$FW"/ .offrails/` then `date -u +"vendored %F" >> .offrails/VERSION`.) An app from before 0.7.1 also gets the `cargo offrails` alias: copy `.offrails/template/app/.cargo/config.toml`'s `[alias]` into the app's `.cargo/config.toml`, and `.gitattributes` from the same place.

## 7. Apply in vertical slices

Each slice must be green under the [Definition of Done](../workflow.md#definition-of-done) before the next one.

**A step applies only where its *Detect* matches.** Many steps are for one kind of app: accounts, live regions, a development pending-migrations page, a CSS banner, channels. Run the detect first; a step for a feature the app doesn't have is **n/a**, listed as such in the plan with the reason, not built "to match". Where a step's proof needs something the app lacks, use the alternative it names (no accounts: `run.sh a11y` instead of `run.sh login`; its preflight still fails on an untrusted certificate). A detect that would match every app for a change that only some need is a changelog bug: apply what clearly applies, and say so in your report. Log `YYYY-MM-DD: upgraded Off Rails <old> → <new>` and each deviation in `docs/decisions.md`.

## 7b. Offer the parts

From 0.7.0, the parts most apps have (mailer, dev mailbox, jobs, jobs dashboard, accounts, realtime, a dev dashboard) exist as tested, security-reviewed code: the samples, shipped in every release as parts in `.offrails/template/parts/` ([samples](../guides/samples.md)). For an app that built these from the guides, they're often better than what it grew, and sometimes they aren't. **Recommend the parts that fit**, show the human what they get (<https://github.com/rust-off-rails/samples> has screenshots of every screen), and install only what they say yes to.

Skip the step when `docs/decisions.md` records an earlier answer (unless the parts gained something since; then offer only that). In a non-interactive run, install nothing: put the offer in the report so the human can answer later.

1. **Find the areas the app has.** Run each detect:

   | Area | Detect | Sample |
   |------|--------|--------|
   | Mailer | `ls src/kernel/mailer.rs src/mailers/mod.rs` | `mailer` |
   | Dev mailbox | `grep -rln '"/dev/mailbox' src` | `dev-mailbox` |
   | Jobs | `ls -d src/kernel/jobs` | `jobs` |
   | Jobs dashboard | `grep -rln '"/admin/jobs' src` | `jobs-dashboard` |
   | Accounts | the `ACCOUNTS:` header line isn't `none`; passwords: `grep -rln password_digest src/models`; magic links: `ls src/models/login_link.rs`; two-factor: `grep -rli totp src` | `auth-core` + `auth-password` / `auth-magic-link`, `auth-totp` |
   | Realtime | `ls -d src/channels src/kernel/channels.rs src/kernel/pubsub.rs` | `realtime` |
   | Dev dashboard | `grep -rn '"/dev/dashboard' src` (usually nothing: it's new; recommend it for every app) | `dev-dashboard` |

2. **Compare honestly.** For each area found, read the sample's README (first section and *Adapting it*) and look at what the app's version already does. Write one or two sentences per area: what the sample would add **to this app**, and what adopting costs (the files it replaces, tables to reconcile, new routes, more code to own). Where the app already does it, say so, and don't offer what it doesn't need. The [samples table](../guides/samples.md#the-samples) has the headline features: a mailbox with HTML, text and source views and a 320 px phone lint; jobs on the database with a dashboard that groups errors by root cause; accounts with TOTP, recovery codes, magic link + code and an account security page; channels with presence and authorization on every join; a LiveDashboard-style dev dashboard with traces, SQL with N+1 detection, a log tail and a routes explorer.
3. **Ask once**, with the plan (step 5), as one question per area: **adopt** (install the sample in place of the app's version), **borrow** (port named ideas into the app's own code) or **skip**. Be enthusiastic about what's genuinely better and plain about the cost. For example:

   > This release brings parts: tested, security-reviewed code for things this app has or should have. See every screen first: https://github.com/rust-off-rails/samples. I recommend these; nothing changes unless you say yes.
   > - **Dev mailbox:** ours shows each email's HTML and text. The sample adds a live inbox, phone-width (320 px) and dark previews that flag mail that breaks in real inboxes, one-key copy of sign-in links and codes, search and a CLI. It replaces `kernel/mailbox.rs` and its templates; stored mail stays.
   > - **Jobs dashboard:** ours lists jobs and retries them. The sample groups failures by root cause, charts throughput and queue wait, and shows what's running now. It replaces `kernel/jobs/web.rs` and its templates; no schema change.
   > - **Dev dashboard (new):** charts, request traces, every SQL query with N+1 warnings and EXPLAIN, a live log, and a routes explorer at `/dev/dashboard`, in development only. It's new code with no change to existing pages.
   >
   > For each: adopt it, borrow specific ideas into our code, or skip?

4. **Do what they chose**, after the convergence slices of step 7, one slice per sample:
   - **Adopt:** `cargo offrails add-part <name>` (dependencies come first), per [samples → Using one](../guides/samples.md#using-one). It refuses, changing nothing, when an anchor is missing or a file it brings exists and differs: add the anchors from [new → anchors](new.md#anchors) and port the template pieces it names (an app built from the guides before 0.7.1 often needs this), or apply the wiring by hand from the part's README. Never copy a migration for a table the app already has: reconcile it with a new migration. Keep the app's working code until the part's tests **and** the app's are green with the part in place; then delete what it replaced. Add its CHECKS.md rows to the relevant feature spec's `## Security` table. Then the Definition of Done on SQLite and Postgres.
   - **Borrow:** port each named idea into the app's own code, with a test, as an ordinary change.
   - **Skip:** nothing to build.
   - Never replace working code without that yes, and never adopt a sample for an area the human didn't pick.
5. **Record** each answer: `YYYY-MM-DD: parts offered on upgrade to <version>: dev-mailbox adopted (v<x>), jobs-dashboard borrowed (error groups), dev-dashboard skipped` (`add-part` also logs each install).

## 8. Verify nothing is stale

- `sh .offrails/tools/offrails-check` exits 0. Its `FAIL` lines are convergence work for this upgrade, not pre-existing debt to report and leave. Two that older specs and configs hit:
  - **`n/a` threat rows.** *Detect:* `grep -nE '^\|[^|]*\|[[:space:]]*(n/a|N/A|none|—)[[:space:]]*\|' product/features/*.md`, or `[security] … says "n/a" with no reason` / `names no test`. *Converge:* a STRIDE category that can't apply becomes `| <Category> | n/a: <why it can't happen here> | — | — |`; a row that names a real threat gets its test written and named, never `—`.
  - **`.env.example` gaps.** *Detect:* `[env] .env.example is missing variable(s)` (the old DoD grep only matched `var("…")`, so variables read through `parse("X", …)` or another helper were never checked). *Converge:* add each as `NAME=value` or `# NAME=value` with a comment ([config](../guides/config.md#envexample)); a `"SCREAMING_SNAKE"` literal in `src/config.rs` that isn't an env var gets a trailing `// not an env var`.
- **Reconcile `docs/decisions.md`.** Read every line. For each decision this release makes obsolete (a workaround for a bug the release fixes, a deviation the framework now adopts, a limit the release resizes), add a line that supersedes it by date (`YYYY-MM-DD: supersedes 2026-10-04 (stall watchdog): 0.6.0 serves HTTP/2, removed the watchdog`) and remove the code it justified in the same slice. Never edit or delete the old line ([workflow](../workflow.md#decisions)). Keep a workaround only if you can still reproduce its evidence on the new release, and say so in the superseding line.
- Copy the app header's `NEVER` list from [`blueprints/AGENTS.md`](../blueprints/AGENTS.md) into the app's `AGENTS.md` when it's missing or older.

- Grep the app (templates, views, `web/src`, docs, tests) for the old version string. Anything that displays the framework version must read `.offrails/VERSION`, never a literal.
- Run the app's own post-upgrade checks (its `AGENTS.md`, "Post-upgrade checks"). If the app's `AGENTS.md` predates that section, add it from [`blueprints/AGENTS.md`](../blueprints/AGENTS.md) and list anything this upgrade taught you to verify next time.
- `bash "$FW/hooks/fingerprint.sh" .offrails` now equals the framework's.

## 9. Report

What changed, in product terms where it matters, plus the deviations you kept.
