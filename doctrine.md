# The Off Rails Doctrine

The Rails Doctrine assumed a human at the keyboard. We kept its pillars and rewrote each one for an agent.

## 0. All code is written by AI

This is the rule the others rest on. **Every line of code in an Off Rails app is written by an AI agent:** Rust, SQL, HTML, CSS, TypeScript, Svelte, YAML, Dockerfiles and scripts. Humans write intent (specs in `product/`, answers in the interview, feedback in conversation) and never code.

It isn't a style preference. It's what makes the rest of this doctrine work:
- boilerplate is free, so explicit beats magic;
- conventions are enforced by the reader that matters, the agent;
- specs stay the source of truth, because code is always regenerated from intent, never hand-patched around it.

When a human edits code anyway, the agent doesn't fight it. It keeps the change working, records the intent behind it in the spec, and from then on the agent owns that code again ([workflow](workflow.md#human-code-edits)).

## 1. Optimize for agent correctness (was: programmer happiness)

Humans wanted short code. Agents want **code whose mistakes are caught before production**. Every choice trades terseness for checkability:

- Queries are typed Rust (SeaORM columns and entities), not strings.
- Templates are type-checked at compile time (Askama), and React props are type-checked against Rust (`ts-rs` → `tsc`).
- Routes, params and forms are typed structs, never string bags.
- What the type system can't prove, the test suite proves, **on both SQLite and Postgres**.

When an agent errs, the tight loop (`cargo check` → error → fix; `cargo nextest run` → failure → fix) catches it in seconds instead of in production.

## 2. Convention over deliberation (was: convention over configuration)

Configuration was never the real cost. Decisions were. An agent left to choose will reconsider the ORM, the template engine and the folder layout on every task, and it will choose differently each time. This framework makes those choices once. **The worst outcome is inconsistency, not a suboptimal choice.**

## 3. Ask about the product, decide about the code

Agents must ask the human the questions only a human can answer (who can see this, what happens on delete, what does it cost) and must never ask questions the framework already answers. See [`interview.md`](interview.md).

## 4. The menu is omakase (still)

One crate per job. See [`stack.md`](stack.md). Substitutions are not taken at the table. The one choice offered is the frontend mode, made once per app.

## 5. One shape

Data lives in the database, logic lives in model functions, HTTP lives in handlers, and the UI lives in templates or React pages. Every resource has the same shape, so an agent that has seen one resource has seen them all.

## 6. Explicit is cheap now (was: exalt beautiful code)

Rails hid boilerplate behind metaprogramming because typing was expensive. Typing is free now. Reading is what costs, for the next agent and for the human reviewing the diff. So:

- No callbacks. If saving a post sends an email, the `create` function enqueues the mailer job, and you can see it.
- No macros beyond what blessed crates provide.
- Repetition across resources is fine, and greppable. Don't abstract until a third copy *and* a spec reason exist.

## 7. Markdown is the spec; the template is its rendering

The guides are the spec: what each part must do, and why. Where Rails would ship a gem (sessions, a job queue, a mailer), Off Rails ships that spec **and its rendering as code you copy**: [`template/app/`](template/README.md), a complete app proven on SQLite and Postgres, and the optional parts (the [samples](guides/samples.md)), added with one command. There is no library and no runtime link back: once copied, the code is the app's, read, adapted and owned by its agent like code it wrote. Humans still write none of it (§0). Snippets in the guides are teaching excerpts; where one disagrees with the template file, the tested template wins, and the guide is the bug to fix. One exception: where a guide states a security invariant and the template does something weaker, the template is the bug. Security invariants never ratchet down to match code.

(Until 0.7.0 the framework shipped Markdown only and agents retyped the skeleton from it, token by token. That cost 50+ minutes per app and drifted. 2026-10-17: the skeleton ships as files.)

## 8. Sharp knives, labeled

Raw SQL exists behind a labeled escape hatch (`guides/database.md`). `unsafe` is banned. `unwrap()` is banned outside tests and startup.

## 9. Value integrated systems

One binary serves HTTP, runs jobs, sends mail, pushes live updates and runs its own migrations. One database holds data, jobs, cache, sessions and pub/sub. **The database is the only infrastructure**: SQLite on a laptop, Postgres in production, same code. (Rails 8 learned this with the Solid trifecta. We start there.)

## 10. Progress over stability, but only through the docs

The stack evolves by editing these Markdown files, not by agents improvising. If a blessed crate becomes wrong, change `stack.md`, then regenerate.

## 11. Push up a big tent

The human doesn't need to know Rust. They need to know what they want. `product/` specs are the human's interface, written in plain English.
