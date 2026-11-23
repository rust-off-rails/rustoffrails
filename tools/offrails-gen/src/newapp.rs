//! `new`: a new app from `template/app`, renamed, with the framework vendored into `.offrails/`
//! and the chosen parts added. Nothing networked runs unless parts are asked for (`cargo add`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::parts::{self, AddOptions};
use crate::util::{self, Fail, Result, rel};

/// The template's placeholders. A rename is these replacements over every text file.
pub const CRATE_TOKEN: &str = "offrails_app";
pub const KEBAB_TOKEN: &str = "offrails-app";
pub const TITLE_TOKEN: &str = "Offrails App";
/// The pitch as the tagline: brand.toml's `tagline` (and src/brand.rs's `TAGLINE`, which
/// `config::APP_TAGLINE` is), capitalized, no closing period. Replaced after the two sentence
/// tokens below, which start with it.
pub const TAGLINE_TOKEN: &str = "A new Rust off Rails app";
/// The pitch as a sentence: the default description.
pub const DESCRIPTION_TOKEN: &str = "A new Rust off Rails app.";
/// The home page's sentence: replaced by the pitch's sentence too.
pub const HOME_TOKEN: &str = "A new Rust off Rails app, generated from the framework template.";
/// blueprints/AGENTS.md's one mode-dependent phrase (the Vite URL of the TS modes).
const TS_ONLY_TOKEN: &str = "{{TS modes only: \", or :5173 via Vite\"; htmx: nothing}}";

/// Top-level framework paths that never go into an app's `.offrails/` (generators/new.md step 1).
/// hooks/fingerprint.sh compares by content with the same list.
const VENDOR_EXCLUDES: [&str; 9] = [
    ".git",
    ".claude-plugin",
    "skills",
    "hooks",
    "evals",
    "brand",
    ".in_use",
    "media",
    // The tarball checksum install.sh writes into ~/.offrails/<version>/: not framework content.
    ".sha256",
];

pub struct NewOptions {
    pub dir: PathBuf,
    pub crate_name: String,
    pub title: String,
    pub pitch: Option<String>,
    pub parts: Vec<String>,
    pub mode: String,
    pub prod_db: String,
    pub git: bool,
    pub offline: bool,
    /// `--invite`: an invite-only app (`DEFAULT_SIGNUP = Signup::Invite`, `ACCOUNTS: … invite`).
    pub invite: bool,
    /// `Snapshot::provenance`: recorded in .offrails/VERSION and the log.
    pub provenance: Option<String>,
}

/// `new --invite`: nobody signs up; `app task auth_invite <email>` creates accounts. The auth parts
/// read `DEFAULT_SIGNUP` (src/config.rs, from auth-core; `APP_AUTH_SIGNUP` overrides it per deploy).
fn make_invite_only(app: &Path) -> Result<()> {
    let config = app.join("src").join("config.rs");
    let text = util::read_text(&config)?;
    let open = "pub const DEFAULT_SIGNUP: Signup = Signup::Open;";
    if !text.contains(open) {
        return Err(Fail::new(format!(
            "--invite: {} has no `{open}` (auth-core older than 0.7.3?)",
            config.display()
        )));
    }
    util::write_text(
        &config,
        &text.replace(open, "pub const DEFAULT_SIGNUP: Signup = Signup::Invite;"),
    )?;
    let agents = app.join("AGENTS.md");
    if agents.is_file() {
        let mut text = util::read_text(&agents)?;
        let value = util::header_value(&text, "ACCOUNTS").unwrap_or_default();
        let (methods, _, rest) = parts::parse_accounts(&value);
        util::set_header(
            &mut text,
            "ACCOUNTS",
            &parts::format_accounts(&methods, true, &rest),
        );
        util::write_text(&agents, &text)?;
    }
    println!(
        "  invite-only: DEFAULT_SIGNUP = Signup::Invite in src/config.rs; accounts come from `cargo run -- task auth_invite <email>`"
    );
    Ok(())
}

pub fn validate_crate_name(name: &str) -> Result<()> {
    let ok = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !name.ends_with('_');
    if !ok {
        return Err(Fail::usage(format!(
            "crate name '{name}' must be snake_case: a-z, 0-9 and _, starting with a letter (e.g. bookshelf)"
        )));
    }
    if [
        "migration",
        "app",
        "test",
        "tests",
        "std",
        "core",
        "self",
        "crate",
        "main",
    ]
    .contains(&name)
    {
        return Err(Fail::usage(format!(
            "crate name '{name}' is reserved here; pick a distinctive one"
        )));
    }
    Ok(())
}

pub fn validate_title(title: &str) -> Result<()> {
    if title.trim().is_empty()
        || title
            .chars()
            .any(|c| matches!(c, '"' | '\\' | '{' | '}' | '<' | '>' | '&' | '\n' | '\r'))
        || title.chars().any(char::is_control)
    {
        return Err(Fail::usage(format!(
            "display name '{title}' must be non-empty plain text (no quotes, backslashes, ampersands, braces or angle brackets)"
        )));
    }
    Ok(())
}

/// The pitch lands inside Rust string literals, Askama views, HTML and a `<meta>` tag, so it must
/// be plain one-line text. Returns the trimmed pitch.
pub const PITCH_MAX_CHARS: usize = 140;
pub fn validate_pitch(pitch: &str) -> Result<String> {
    let p = pitch.trim();
    if p.is_empty()
        || p.chars().count() > PITCH_MAX_CHARS
        || p.chars()
            .any(|c| c.is_control() || matches!(c, '"' | '\\' | '{' | '}' | '<' | '>' | '&'))
    {
        return Err(Fail::usage(format!(
            "--pitch must be 1 to {PITCH_MAX_CHARS} characters of plain one-line text (no quotes, backslashes, ampersands, braces, angle brackets or control characters)"
        )));
    }
    Ok(p.to_owned())
}

/// Replace the template tokens in one text.
pub fn rename(text: &str, krate: &str, title: &str, pitch: Option<&str>) -> String {
    let kebab = krate.replace('_', "-");
    let mut s = text
        .replace(CRATE_TOKEN, krate)
        .replace(KEBAB_TOKEN, &kebab)
        .replace(TITLE_TOKEN, title);
    if let Some(p) = pitch {
        let (tagline, sentence) = pitch_forms(p);
        s = s
            .replace(HOME_TOKEN, &sentence)
            .replace(DESCRIPTION_TOKEN, &sentence)
            .replace(TAGLINE_TOKEN, &tagline);
    }
    s
}

/// The pitch as the tagline (capitalized, no closing period: brand.toml's `tagline`, the home
/// page title "App: Track the books you read", the social card) and as a sentence (with its
/// period: the description, the home page).
pub fn pitch_forms(pitch: &str) -> (String, String) {
    let typed = pitch.trim().trim_end_matches('.');
    let mut chars = typed.chars();
    let tagline = match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    };
    let sentence = if tagline.is_empty() {
        String::new()
    } else {
        format!("{tagline}.")
    };
    (tagline, sentence)
}

/// Build outputs and local state that never come from the template, even when someone built it in place.
fn skip_template_path(rel: &str) -> bool {
    let first = rel.split('/').next().unwrap_or("");
    rel.split('/')
        .any(|seg| seg == "target" || seg == "node_modules" || seg == ".DS_Store")
        || rel == ".env"
        || rel == "static/app.css"
        || rel == "bin/tailwindcss"
        || (first == "storage" && rel != "storage/.keep")
        || rel.starts_with(".zap/zap-report")
        || rel == ".zap/zap.yaml"
}

fn dir_is_empty_enough(dir: &Path) -> Result<bool> {
    if !dir.exists() {
        return Ok(true);
    }
    for entry in util::ctx(fs::read_dir(dir), "reading", dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        // target/: `cargo run --target-dir <dir>/target/offrails-gen` builds this tool there first.
        if name != "target" && name != ".git" && name != ".DS_Store" {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The framework to copy from (`new`, `vendor`). Dropping it removes its temporary export.
pub struct Snapshot {
    pub root: PathBuf,
    /// Where it came from when that isn't a release install: `from git v0.7.3 (<sha>)`, `from git
    /// <sha> (unreleased: …)` or `from the working tree of <sha>…`. `.offrails/VERSION`'s third
    /// line and the `new` log line. None: the plugin, ~/.offrails/current, a tag export.
    pub provenance: Option<String>,
    tmp: Option<PathBuf>,
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        if let Some(t) = &self.tmp {
            let _ = fs::remove_dir_all(t);
        }
    }
}

/// A release install is used as it is. A git checkout (`<fw>/.git`) is never vendored as a moving
/// working tree: HEAD's committed files are (`git archive HEAD`, as tools/release/release.sh
/// builds the release tarball), and the commit is recorded. At the `v<VERSION>` tag that is the
/// release; on a release branch before tagging VERSION still names the previous release, so the
/// record says "unreleased". `worktree`: the working tree as it is, uncommitted changes included
/// (the framework's own CI), recorded as such.
pub fn snapshot(fw: &Path, worktree: bool) -> Result<Snapshot> {
    let as_is = |provenance| Snapshot {
        root: fw.to_path_buf(),
        provenance,
        tmp: None,
    };
    if !fw.join(".git").exists() || !util::have("git") {
        return Ok(as_is(None));
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(fw)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let Some(head) = git(&["rev-parse", "HEAD"]) else {
        return Ok(as_is(None));
    };
    let short = head.get(..12).unwrap_or(&head).to_owned();
    let tag = format!("v{}", util::framework_version(fw)?);
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    if worktree {
        let p = format!(
            "from the working tree of {short}{}",
            if dirty {
                ", uncommitted changes included"
            } else {
                ""
            }
        );
        println!(
            "  framework {} is a git checkout; --worktree: using it as it is, {p}",
            fw.display()
        );
        return Ok(as_is(Some(p)));
    }
    let tagged = git(&[
        "rev-parse",
        "-q",
        "--verify",
        &format!("refs/tags/{tag}^{{commit}}"),
    ])
    .is_some_and(|c| c == head);
    let provenance = if tagged {
        format!("from git {tag} ({short})")
    } else {
        format!(
            "from git {short} (unreleased: HEAD is not the {tag} tag; VERSION names the last release)"
        )
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!(
        "offrails-fw-{}-{nanos}-{short}",
        std::process::id()
    ));
    util::ctx(fs::create_dir_all(&tmp), "creating", &tmp)?;
    let snap = Snapshot {
        root: tmp.clone(),
        provenance: Some(provenance),
        tmp: Some(tmp.clone()),
    };
    let mut archive = util::ctx(
        Command::new("git")
            .arg("-C")
            .arg(fw)
            .args(["archive", "--format=tar", "HEAD"])
            .stdout(Stdio::piped())
            .spawn(),
        "running git archive in",
        fw,
    )?;
    let tar = Command::new("tar")
        .arg("-x")
        .arg("-f")
        .arg("-")
        .arg("-C")
        .arg(&tmp)
        .stdin(archive.stdout.take().map_or_else(Stdio::null, Stdio::from))
        .status();
    let archived = archive.wait().is_ok_and(|s| s.success());
    if !archived || !tar.is_ok_and(|s| s.success()) {
        return Err(Fail::new(format!(
            "`git archive HEAD | tar -x` of {} failed: export it by hand (git -C <fw> archive HEAD | tar -x -C <tmp>) and pass --framework <tmp>",
            fw.display()
        )));
    }
    println!(
        "  framework {} is a git checkout: using HEAD's committed files (git archive HEAD), {}",
        fw.display(),
        snap.provenance.as_deref().unwrap_or_default()
    );
    if dirty {
        println!(
            "  note: its uncommitted changes are not used (commit them first, or pass --worktree)"
        );
    }
    Ok(snap)
}

/// Copy the framework into `<app>/.offrails/` (generators/new.md step 1), then date the vendoring
/// (and say where it came from: `snapshot`).
pub fn vendor(fw: &Path, app: &Path, provenance: Option<&str>) -> Result<()> {
    let dest = app.join(".offrails");
    if dest.exists() {
        util::ctx(fs::remove_dir_all(&dest), "removing", &dest)?;
    }
    let skip_dir = |d: &str| {
        VENDOR_EXCLUDES.contains(&d)
            || d.ends_with("/target")
            || d.ends_with("/node_modules")
            || d == "target"
    };
    for f in util::files_under_except(fw, &skip_dir)? {
        let first = f.split('/').next().unwrap_or("");
        if VENDOR_EXCLUDES.contains(&first)
            || f.split('/').any(|s| s == ".DS_Store")
            || f.starts_with("tools/offrails-gen/target/")
            || f.starts_with("template/app/target/")
        {
            continue;
        }
        util::copy_file(&rel(fw, &f), &rel(&dest, &f))?;
    }
    let version = util::framework_version(fw)?;
    util::write_text(
        &dest.join("VERSION"),
        &format!(
            "{version}\nvendored {}\n{}",
            util::today_utc(),
            provenance.map(|p| format!("{p}\n")).unwrap_or_default()
        ),
    )?;
    Ok(())
}

pub fn run(fw: &Path, o: &NewOptions) -> Result<()> {
    validate_crate_name(&o.crate_name)?;
    validate_title(&o.title)?;
    let pitch = o.pitch.as_deref().map(validate_pitch).transpose()?;
    match o.mode.as_str() {
        "htmx" | "api" | "inertia" | "spa" => {}
        m => {
            return Err(Fail::usage(format!(
                "--mode {m}: expected htmx | inertia | spa | api"
            )));
        }
    }
    match o.prod_db.as_str() {
        "postgres" | "sqlite" => {}
        d => {
            return Err(Fail::usage(format!(
                "--prod-db {d}: expected postgres | sqlite"
            )));
        }
    }
    if !dir_is_empty_enough(&o.dir)? {
        return Err(Fail::usage(format!(
            "{} exists and isn't empty; `new` only writes into a new or empty directory (use add-part in an existing app)",
            o.dir.display()
        )));
    }
    let catalog = parts::catalog(fw)?;
    for p in &o.parts {
        if p != "all" && !catalog.contains_key(p) {
            return Err(Fail::usage(format!(
                "--parts: no part named '{p}' (list-parts shows them)"
            )));
        }
    }
    let auth = o
        .parts
        .iter()
        .any(|p| matches!(p.as_str(), "all" | "auth-password" | "auth-magic-link"));
    if o.invite && !auth {
        return Err(Fail::usage(
            "--invite needs a sign-in part: --parts auth-password (or auth-magic-link)",
        ));
    }
    let app = &o.dir;
    let template = fw.join("template").join("app");
    let version = util::framework_version(fw)?;
    let today = util::today_utc();

    println!(
        "new {} ({}) from Rust off Rails {version} template -> {}",
        o.crate_name,
        o.title,
        app.display()
    );
    let mut count = 0;
    for f in util::files_under_except(&template, &|d: &str| {
        d == "target" || d.ends_with("/node_modules")
    })? {
        if skip_template_path(&f) {
            continue;
        }
        let src = rel(&template, &f);
        let dest = rel(app, &f);
        util::copy_file(&src, &dest)?;
        let bytes = fs::read(&src)?;
        if let Some(text) = util::as_text(&bytes) {
            let renamed = rename(text, &o.crate_name, &o.title, pitch.as_deref());
            if renamed != text {
                util::write_text(&dest, &renamed)?;
            }
        }
        count += 1;
    }
    println!(
        "  copy {count} files from template/app (tokens {CRATE_TOKEN}, {KEBAB_TOKEN}, \"{TITLE_TOKEN}\" renamed)"
    );

    vendor(fw, app, o.provenance.as_deref())?;
    println!(
        "  vendor .offrails/ (Rust off Rails {version}, vendored {today}{})",
        o.provenance
            .as_deref()
            .map(|p| format!(", {p}"))
            .unwrap_or_default()
    );
    if cfg!(target_os = "macos") {
        // Spotlight would index every build artifact: gigabytes of churn, a busy disk.
        util::write_text(&app.join("target").join(".metadata_never_index"), "")?;
    }

    // AGENTS.md and product/README.md come from the blueprints, the one copy of each.
    let mut agents = util::read_text(&fw.join("blueprints").join("AGENTS.md"))?;
    let api = if o.mode == "api" { "rest" } else { "none" };
    let header: [(&str, String); 11] = [
        ("APP", o.crate_name.clone()),
        (
            "FRAMEWORK",
            format!("Rust off Rails {version} (.offrails/, vendored {today})"),
        ),
        ("FRONTEND_MODE", "htmx".into()),
        (
            "UI_LIBRARY",
            "none   (none = no JS UI: htmx without islands, or api)".into(),
        ),
        ("ISLANDS", "no".into()),
        (
            "API",
            format!("{api}   (a versioned API for other programs; api mode is never none)"),
        ),
        (
            "DEV_DATABASE",
            "sqlite (storage/development.sqlite3)".into(),
        ),
        ("PROD_DATABASE", o.prod_db.clone()),
        ("DEPLOY_TARGET", "later".into()),
        ("ACCOUNTS", "none  ROLES: none  TENANCY: none".into()),
        (
            "EMAIL",
            "none (no mailer yet: cargo offrails add-part mailer adds it, with the dev mailbox)"
                .into(),
        ),
    ];
    for (k, v) in &header {
        if !util::set_header(&mut agents, k, v) {
            return Err(Fail::new(format!("blueprints/AGENTS.md has no {k}: line")));
        }
    }
    // htmx (what `new` writes) has no Vite dev server; the TS modes' conversion adds the phrase.
    agents = agents.replace(TS_ONLY_TOKEN, "");
    util::write_text(&app.join("AGENTS.md"), &agents)?;
    let product = util::read_text(&fw.join("blueprints").join("product-README.md"))?
        .replace("{{App Name}}", &o.title)
        .replace("{{YYYY-MM-DD}}", &today);
    util::write_text(&rel(app, "product/README.md"), &product)?;
    println!(
        "  write AGENTS.md, product/README.md (from .offrails/blueprints; fill them from the interview)"
    );

    let parts_note = if o.parts.is_empty() {
        "none".to_owned()
    } else {
        o.parts.join(",")
    };
    util::log_decision(
        app,
        &format!(
            "app generated from the Rust off Rails {version} template (cargo offrails new; mode=htmx, prod db={}, parts={parts_note}){}",
            o.prod_db,
            o.provenance
                .as_deref()
                .map(|p| format!("; framework vendored {p}"))
                .unwrap_or_default()
        ),
    )?;
    if o.mode != "htmx" {
        util::log_decision(
            app,
            &format!(
                "FRONTEND_MODE {} requested: generated the htmx template, to be converted per .offrails/template/README.md#other-modes (the header says htmx until the code matches)",
                o.mode
            ),
        )?;
    }

    if !o.parts.is_empty() {
        println!();
        parts::add(
            fw,
            app,
            &o.parts,
            &AddOptions {
                force: false,
                dry_run: false,
                offline: o.offline,
                fmt: true,
                quiet_next: true,
            },
        )?;
    }

    if o.invite {
        make_invite_only(app)?;
    }

    // brand.toml carries the new name: the theme CSS, src/brand.rs, the favicon set and the card
    // (after the parts: the mailer's layout takes the brand's email colors).
    if app.join("brand.toml").is_file() {
        crate::brand::run(
            app,
            &crate::brand::Options {
                check: false,
                images: true,
                init: false,
                quiet: true,
            },
        )?;
    }

    if o.parts.is_empty() && util::have("cargo") {
        // The new crate name sorts differently in `use` lists, and a pitch rewraps a line.
        if util::run(app, "cargo", &["fmt", "--all"]).is_err() {
            eprintln!("note: `cargo fmt --all` failed; run it before the first commit");
        }
    }

    if o.git && !app.join(".git").exists() {
        if util::have("git") {
            util::run(app, "git", &["init", "-q"])?;
            println!(
                "  git init (no commit yet: the baseline commit comes after the first green run)"
            );
        } else {
            println!("  note: git not found; run `git init` yourself");
        }
    }

    println!("\ndone. Next, in {}:", app.display());
    println!(
        "  bin/setup-tailwind && cargo offrails verify   # fmt, CSS, clippy, SQLite + Postgres (first build: minutes)"
    );
    println!(
        "  cargo offrails verify --full               # + offrails-check, coverage, deny, gitleaks"
    );
    println!(
        "  bin/setup && bin/dev                       # Windows: see .offrails/template/README.md#windows"
    );
    println!(
        "  edit brand.toml, then cargo offrails brand   # colors, fonts, radius, logo: theme, favicon set, social card"
    );
    if o.mode != "htmx" {
        println!(
            "  convert to {}: .offrails/template/README.md#other-modes",
            o.mode
        );
    }
    println!("  then write only the app's own code: product/ specs first, cargo offrails add-part");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_replaces_every_token() {
        let t = "name = \"offrails_app\"\nimage: offrails-app:local\nAPP_NAME = \"Offrails App\"\ntest_offrails_app_x\n";
        assert_eq!(
            rename(t, "book_shelf", "Book Shelf", None),
            "name = \"book_shelf\"\nimage: book-shelf:local\nAPP_NAME = \"Book Shelf\"\ntest_book_shelf_x\n"
        );
        let p = "tagline = \"A new Rust off Rails app\"\nDESC = \"A new Rust off Rails app.\";\n<p>A new Rust off Rails app, generated from the framework template.</p>\n<title>Offrails App: A new Rust off Rails app</title>\n";
        assert_eq!(
            rename(p, "x", "X", Some("track the books you read")),
            "tagline = \"Track the books you read\"\nDESC = \"Track the books you read.\";\n<p>Track the books you read.</p>\n<title>X: Track the books you read</title>\n"
        );
        assert_eq!(
            pitch_forms("Notes from the field."),
            (
                "Notes from the field".to_owned(),
                "Notes from the field.".to_owned()
            )
        );
        assert_eq!(
            pitch_forms("notes on software, written slowly"),
            (
                "Notes on software, written slowly".to_owned(),
                "Notes on software, written slowly.".to_owned()
            ),
            "one tagline, capitalized like brand.toml's, whatever case was typed"
        );
    }

    /// The installer's tarball checksum (`~/.offrails/<version>/.sha256`) is not vendored: the
    /// plugin's copy has none, so the app would look drifted (#15).
    #[test]
    fn vendoring_skips_the_installer_checksum() {
        let base = std::env::temp_dir().join(format!("offrails-vendor-sha-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (fw, app) = (base.join("fw"), base.join("app"));
        fs::create_dir_all(fw.join("guides")).unwrap();
        fs::create_dir_all(&app).unwrap();
        fs::write(fw.join("VERSION"), "0.7.5\n").unwrap();
        fs::write(fw.join("guides/a.md"), "doc\n").unwrap();
        fs::write(fw.join(".sha256"), "3f9c\n").unwrap();
        vendor(&fw, &app, None).unwrap();
        assert!(app.join(".offrails/guides/a.md").is_file());
        assert!(!app.join(".offrails/.sha256").exists());
        let _ = fs::remove_dir_all(&base);
    }

    /// A framework checkout is vendored from HEAD, never its working tree, and says which commit
    /// and whether it is the release tag (generators/new.md step 1).
    #[test]
    fn a_framework_checkout_is_vendored_from_head_with_its_commit() {
        if !util::have("git") || !util::have("tar") {
            return;
        }
        let dir = std::env::temp_dir().join(format!("offrails-snap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("template/app")).unwrap();
        fs::write(dir.join("VERSION"), "0.7.2\n").unwrap();
        fs::write(dir.join("template/app/a.txt"), "committed\n").unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        git(&["commit", "-qm", "one"]);
        git(&["tag", "v0.7.2"]);
        // At the tag: the release, recorded with its tag.
        let snap = snapshot(&dir, false).unwrap();
        assert!(
            snap.provenance
                .as_deref()
                .is_some_and(|p| p.starts_with("from git v0.7.2 (")),
            "{:?}",
            snap.provenance
        );
        drop(snap);
        // A release branch before tagging: HEAD past the tag, VERSION unchanged, a dirty tree.
        fs::write(dir.join("template/app/a.txt"), "next\n").unwrap();
        git(&["commit", "-qam", "two"]);
        fs::write(dir.join("template/app/a.txt"), "uncommitted\n").unwrap();
        let snap = snapshot(&dir, false).unwrap();
        let root = snap.root.clone();
        assert_ne!(root, dir);
        assert_eq!(
            // `git archive` honours core.autocrlf, which Windows sets: CRLF there, as in a checkout.
            fs::read_to_string(root.join("template/app/a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "next\n",
            "HEAD's files, not the working tree's"
        );
        assert!(
            snap.provenance
                .as_deref()
                .is_some_and(|p| p.contains("unreleased") && p.contains("v0.7.2")),
            "{:?}",
            snap.provenance
        );
        let app = dir.join("app");
        vendor(&root, &app, snap.provenance.as_deref()).unwrap();
        let version = fs::read_to_string(app.join(".offrails/VERSION")).unwrap();
        let lines: Vec<&str> = version.lines().collect();
        assert_eq!(lines[0], "0.7.2", "VERSION's first line stays the version");
        assert!(lines[1].starts_with("vendored "));
        assert!(lines[2].starts_with("from git ") && lines[2].contains("unreleased"));
        drop(snap);
        assert!(!root.exists(), "the export is removed");
        // --worktree: the working tree as it is, said so.
        let snap = snapshot(&dir, true).unwrap();
        assert_eq!(snap.root, dir);
        assert!(
            snap.provenance
                .as_deref()
                .is_some_and(|p| p.contains("uncommitted changes included"))
        );
        drop(snap);
        // Not a checkout (a release install): as it is, nothing recorded.
        let plain = dir.join("app/.offrails");
        let snap = snapshot(&plain, false).unwrap();
        assert_eq!(
            (snap.root.as_path(), snap.provenance.as_deref()),
            (plain.as_path(), None)
        );
        drop(snap);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_template_still_has_every_token_new_replaces() {
        let home = include_str!("../../../template/app/templates/home/index.html");
        assert!(
            home.contains(HOME_TOKEN),
            "templates/home/index.html lost its pitch sentence"
        );
        let config = include_str!("../../../template/app/src/config.rs");
        assert!(
            config.contains("pub const APP_TAGLINE: &str = crate::brand::TAGLINE;"),
            "config::APP_TAGLINE must be brand.toml's tagline (src/brand.rs), the one source"
        );
        let brand = include_str!("../../../template/app/brand.toml");
        assert!(brand.contains(&format!("tagline = \"{TAGLINE_TOKEN}\"")));
        let views = include_str!("../../../template/app/src/views/mod.rs");
        assert!(views.contains(DESCRIPTION_TOKEN));
        let agents = include_str!("../../../blueprints/AGENTS.md");
        assert!(
            agents.contains(TS_ONLY_TOKEN),
            "blueprints/AGENTS.md's TS phrase changed: update TS_ONLY_TOKEN"
        );
    }

    #[test]
    fn crate_names_are_checked() {
        assert!(validate_crate_name("bookshelf").is_ok());
        assert!(validate_crate_name("book_shelf2").is_ok());
        for bad in [
            "Bookshelf",
            "book-shelf",
            "2books",
            "",
            "migration",
            "books_",
        ] {
            assert!(validate_crate_name(bad).is_err(), "{bad}");
        }
        assert!(validate_title("Book \"Shelf\"").is_err());
        assert!(validate_title("Bookshelf").is_ok());
        assert!(validate_title("A & B").is_err());
        assert!(validate_title("Book\tshelf").is_err());
    }

    #[test]
    fn pitch_must_be_plain_one_line_text() {
        assert_eq!(validate_pitch("  track books  ").unwrap(), "track books");
        for bad in [
            "say \"hi\"",
            "back\\slash",
            "two\nlines",
            "tab\there",
            "x\"; evil(); \"",
            "<b>hi</b>",
            "{{ x }}",
            "a & b",
            "",
            "   ",
        ] {
            assert!(validate_pitch(bad).is_err(), "{bad:?}");
        }
        assert!(validate_pitch(&"x".repeat(140)).is_ok());
        assert!(validate_pitch(&"x".repeat(141)).is_err());
    }

    #[test]
    fn build_outputs_never_come_from_the_template() {
        for p in [
            "target/debug/x",
            ".env",
            "static/app.css",
            "bin/tailwindcss",
            "storage/development.sqlite3",
        ] {
            assert!(skip_template_path(p), "{p}");
        }
        for p in [
            "storage/.keep",
            ".env.example",
            "static/app.js",
            "src/main.rs",
        ] {
            assert!(!skip_template_path(p), "{p}");
        }
    }
}
