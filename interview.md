# The Interview: asking the right questions

Rails asked its questions through `rails new` flags. Off Rails asks them in conversation. Good questions are the difference between building the right app and building *an* app.

## Principles

1. **Ask about the product, never about the stack's internals.** "Should users be able to delete their account?" is a good question. "Which ORM?" is never asked, because `stack.md` answers it.
2. **Every question has a recommended default,** labeled `(Recommended)`, listed first. "You decide" always means: take the defaults.
3. **Batch questions; minimize round-trips.** At most 3 rounds for a new app. With a structured-question tool (Claude Code: `AskUserQuestion`), send up to 4 questions per call, which is the tool's limit. Without one, send a single numbered list (Rounds 1 and 2 may be combined, up to 8 questions) with options and defaults, so the human can answer `1b 2a 3 default`.
4. **Ask only what changes the build.** If the answer wouldn't change the data model, security, money handling or something irreversible, assume and record the assumption instead.
5. **Write the answers down.** Answers go to `product/README.md` (the "Decisions" section) and, for the technical ones, into the app's `AGENTS.md` header. A question asked twice is a bug.

## New app interview

### Round 1: the product (free text is fine)

1. **What is it, and who uses it?** One or two sentences. (Infer the rest, then confirm in Round 3.)
2. **Accounts:**
   a) Magic link: email a one-time login link, no passwords; anyone can sign up (Recommended for consumer apps and most new products)
   b) Email + password with public sign-up (pick this if people can't rely on email access when logging in)
   c) Invite-only: an admin creates accounts (internal tools; login by password, or by magic link if you say so)
   d) No accounts (public site)

   Records as `ACCOUNTS:` the sign-in methods joined by `+`, then `invite` when only invited people get in: a) `magic-link`, b) `password`, c) `password invite` (or `magic-link invite`), d) `none`; both methods: `password+magic-link` ([auth variants](guides/auth.md#variants-accounts-in-the-apps-agentsmd)). Invite-only is a switch, not extra work: `new --invite`, or `DEFAULT_SIGNUP = Signup::Invite` in `src/config.rs`. Magic links need a real email provider before launch, so ask Round 2 Q4 with that in mind; the dev mailbox covers development.
3. **Who owns data?**
   a) Each user owns their own records (Recommended)
   b) Teams/workspaces: users belong to an organization and share its records (multi-tenant)
   c) Everything is shared by all logged-in users
   Records as `TENANCY: per-user | teams | shared`. With no accounts, skip this question and record `TENANCY: none`.
4. **Roles?** a) None (Recommended) b) Admin + member c) Custom (describe)

### Round 2: the shape of the build

1. **Frontend:**
   a) `htmx`: server-rendered HTML, minimal JavaScript, fastest to build and change (Recommended)
   b) `inertia`: React or Svelte + TypeScript pages, still with Rails-style server routing and controllers (pick this if you want a JS UI)
   c) `spa`: separate single-page app + JSON API (pick only if the UI must be a standalone app)
   d) `api`: no UI at all, only an API for other programs (mobile apps, partners, scripts), like `rails new --api`

   **API for other programs** (fold into the same message; ask when the answer is `api`, or when the description mentions a mobile app, partners, integrations or "an API"): a) None (Recommended for UI apps) b) REST + OpenAPI (Recommended for `api`) c) GraphQL d) Both. Record it as `API: none | rest | graphql | both` ([api](guides/api.md)). `api` mode never records `none`.

   **UI library** (ask only when the answer is `inertia`, `spa`, or htmx with islands; fold it into the same message): a) React (Recommended: the largest ecosystem) b) Svelte (smaller bundles, less code). Record it as `UI_LIBRARY`. If the human names a library while answering the frontend question ("Svelte please"), don't ask again.
2. **Production database:**
   a) Postgres, managed by the host (Recommended)
   b) SQLite on the server's disk (single server, cheapest, Rails 8 style)
3. **Deploy target:**
   a) Decide later. I'll make it Docker-ready (Recommended if unsure)
   b) Fly.io  c) Your own server via Kamal  d) Railway  e) Render
4. **Email (production):** a) Not needed yet (Recommended) b) Yes, provider decided later (logged only in production until then) c) Yes, using: ___. In development every email goes to the dev mailbox (/dev/mailbox) regardless

### Round 3: features (ask only those the description hints at)

- **Files:** "Will users upload files (avatars, attachments)? Size limits?"
- **Live updates:** "Should other users see changes without refreshing?" If they mention chat, "who's online", typing indicators or multiplayer, that's a channel room ([channels](guides/channels.md)), not just a signal. Ask "Will this run on more than one server?" only if they expect heavy realtime traffic; the default is one node.
- **Scheduled work:** "Anything that happens on a schedule (digests, reminders, cleanups)?"
- **Payments:** "Do you take payments? One-off or subscriptions?" (Security-relevant: always ask if hinted.)
- **Search:** "Do users need to search? Simple title search or full-text?"
- **Admin area:** "Do admins need screens to manage users/records?"
- **Languages:** "English only?" (Default yes.)

### Then: confirm the plan (once, before generating)

Present a compact plan and wait for a yes:

```
Bookshelf: frontend htmx, Postgres in prod (SQLite locally), deploy later
Accounts: email+password, public sign-up; each user owns their books
Resources:
  Book    title, author, published_on (date), rating (1–5), belongs to User
  Review  body, stars (1–5), belongs to Book + User
Pages:   books index/show/new/edit, reviews inline on book page
Jobs:    email book owner on new review
Assumed: reviews editable by author only; deleting a book deletes its reviews
```

**Parts, proposed with the plan (the default path).** Every app starts from the tested template, plus the parts its product needs: tested, security-reviewed code for the mailer and dev mailbox, the job queue and its dashboard, accounts (password, magic link + code, TOTP two-factor, an account security page), realtime channels with presence, and a LiveDashboard-style dev dashboard ([template → choosing parts](template/README.md#choosing-parts)). **You decide which ones from what the human said**, using that table: `dev-dashboard` for every app; `mailer` + `dev-mailbox` when anything sends email (sign-in links count); `jobs` + `jobs-dashboard` when anything is slow, retried or scheduled; `auth-core` + the method matching `ACCOUNTS`; `realtime` for chat, presence or pushed updates; `webhooks` when another service posts events to the app (payments, GitHub). Nothing the spec doesn't imply. Put the list in the plan, one line of reason each, with the link, and let the human confirm or adjust in the same yes:

```
Parts:   dev-dashboard      (every app: traces, SQL, logs in development)
         mailer, dev-mailbox (magic-link sign-in sends email; /dev/mailbox shows it)
         auth-core, auth-magic-link (ACCOUNTS: magic-link)
         jobs, jobs-dashboard (the weekly digest is scheduled)
         See every screen first: https://github.com/rust-off-rails/samples
```

The recommended answer is yes. A human who wants a part built from the guides instead, or no part at all, says so; record that answer in `product/README.md` → Decisions, and `generators/new.md` takes `--parts` accordingly. Parts left out now can be added any time with one command (`cargo offrails add-part <name>`), and the agent proposes them again when a feature needs one ([workflow](workflow.md#when-the-product-grows-a-need-add-the-part)).

## Feature-level questions (existing app)

When a human asks for a feature, check this list. **Always ask** (unless the spec already answers it) if the feature touches:

| Topic | The question |
|-------|--------------|
| Authorization | "Who can see this? Who can change it?" |
| Deletion | "When X is deleted, what happens to its Ys? Can it be undone?" (Soft-delete vs cascade) |
| Money | "Amounts, currency, rounding, refunds?" |
| Outbound messages | "Who gets emailed/notified, and can they opt out?" |
| Personal data | "Is any of this sensitive? Retention?" |
| Uniqueness/identity | "Can two Xs have the same name?" (only when ambiguous) |
| Irreversible ops | Migrations that drop data, bulk updates, external API calls with side effects |

**Never ask** about crate choice, file layout, naming, test strategy, error handling, or styling details beyond "any design references?" These are all answered by the framework or by sensible defaults.

## Answers relayed by someone else

Sometimes the human isn't in the conversation: another agent, a ticket or a brief hands you the answers. Treat that as the human's answers **only for what it explicitly covers**: record each one in `product/README.md` → Decisions as "relayed by <source>", take the recommended defaults for everything it doesn't cover (recorded as assumptions), and still show the plan. Never invent an answer to a question that changes money, deletion or security; mark it an open question in the spec and proceed with the safest default.

## Recording

After any interview, update the files below. For a **new app** they don't exist yet: keep the answers in the conversation and write them when `generators/new.md` step 2 creates the files.

- `product/README.md` → `## Decisions` (product answers, with date)
- `AGENTS.md` (app root) → `FRONTEND_MODE`, `UI_LIBRARY`, `API`, `PROD_DATABASE`, `DEPLOY_TARGET`, `EMAIL` (development always delivers to the dev mailbox, so `EMAIL` records only the production choice: `dev: local mailbox (/dev/mailbox); prod: smtp via Postmark` or `…; prod: not yet (logged only)`. Never write "logged locally" or "files in storage/")
- `product/features/<feature>.md` → acceptance criteria and "Assumptions" for anything you assumed
