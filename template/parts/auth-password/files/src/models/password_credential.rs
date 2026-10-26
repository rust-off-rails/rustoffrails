//! Passwords: argon2id hashes, one per account, in their own table so password sign-in combines
//! with magic links and two-factor without touching `users`.
//!
//! GUIDE: .offrails/guides/auth.md#passwords-password-variants
//!
//! - argon2id with OWASP's parameters (`PARAMS`), a random salt per hash, the PHC string stored.
//! - Hashing and verifying are CPU-heavy, so they run on the blocking pool, never the executor.
//! - A hash made with older parameters is upgraded on the next successful sign-in.
//! - An unknown email (or an account without a password) still runs exactly one verify, against
//!   a dummy hash, so the response time doesn't reveal whether the account exists.

use std::sync::{
    LazyLock,
    atomic::{AtomicU64, Ordering},
};

use argon2::{Algorithm, Argon2, Params, PasswordHasher, PasswordVerifier, Version};
use chrono::{DateTime, Utc};
use sea_orm::{ActiveValue::Set, entity::prelude::*, sea_query::OnConflict};

use crate::error::AppError;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "password_credentials")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: i64,
    /// The PHC string: `$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`.
    #[sea_orm(column_type = "Text")]
    pub digest: String,
    pub changed_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::UserId",
        to = "super::user::Column::Id",
        on_delete = "Cascade"
    )]
    User,
}

impl Related<super::user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

/// OWASP Password Storage Cheat Sheet, argon2id: m=19 MiB, t=2, p=1 (its first recommended
/// configuration).
// APP: raise memory or iterations when your servers allow (keep a sign-in under ~250 ms).
// Existing hashes upgrade themselves on the next sign-in (`needs_rehash`).
// SECURITY: argon2id is memory-hard: a leaked table costs an attacker GPU memory per guess
// (test: hashes_use_argon2id_with_owasp_parameters)
// INVARIANT: new hashes use argon2id with these parameters (test: hashes_use_argon2id_with_owasp_parameters)
pub const M_COST_KIB: u32 = 19 * 1024;
pub const T_COST: u32 = 2;
pub const P_COST: u32 = 1;

/// APP: argon2 hashes or verifies running at once (each takes about 19 MiB and a blocking
/// thread). More wait up to 5 seconds, then get 429, so a login flood can't exhaust the pool.
pub const MAX_CONCURRENT_ARGON2: usize = 4;

static ARGON2_PERMITS: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| {
        std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_ARGON2))
    });

/// Runs argon2 `work` on the blocking pool, at most `MAX_CONCURRENT_ARGON2` at a time.
// SECURITY: bounded so concurrent sign-ins can't saturate the blocking pool (CPU and memory DoS)
async fn run_argon2<T, F>(work: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
{
    let permit = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        ARGON2_PERMITS.clone().acquire_owned(),
    )
    .await
    .map_err(|_| AppError::TooManyRequests)?
    .map_err(|_| AppError::TooManyRequests)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("argon2 task: {e}")))?
}

fn hasher() -> Result<Argon2<'static>, AppError> {
    let params = Params::new(M_COST_KIB, T_COST, P_COST, None)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("argon2 params: {e}")))?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Hashes on the blocking pool. Returns the PHC string.
pub async fn hash(password: String) -> Result<String, AppError> {
    run_argon2(move || {
        hasher()?
            .hash_password(password.as_bytes())
            .map(|h| h.to_string())
            .map_err(|e| AppError::Internal(anyhow::anyhow!("argon2 hash: {e}")))
    })
    .await
}

/// The outcome of checking a password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checked {
    pub ok: bool,
    /// Right password, but hashed with older parameters: store a fresh hash.
    pub needs_rehash: bool,
}

/// How many argon2 verifies this process has run: the suite proves an unknown email costs one,
/// like a known one (nextest runs each test in its own process, so the count is the test's).
static VERIFIES: AtomicU64 = AtomicU64::new(0);

pub fn verifies_so_far() -> u64 {
    VERIFIES.load(Ordering::Relaxed)
}

/// Verifies on the blocking pool. A malformed digest is a mismatch, never an error.
// SECURITY: argon2's verify compares in constant time
// (test: hashes_use_argon2id_with_owasp_parameters)
pub async fn verify(password: String, digest: String) -> Result<Checked, AppError> {
    run_argon2(move || {
        VERIFIES.fetch_add(1, Ordering::Relaxed);
        let Ok(parsed) = argon2::PasswordHash::new(&digest) else {
            return Ok(Checked {
                ok: false,
                needs_rehash: false,
            });
        };
        // Verify with the parameters inside the stored hash, so old hashes still verify.
        let ok = Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();
        Ok(Checked {
            ok,
            needs_rehash: ok && !current(&digest),
        })
    })
    .await
}

/// Is this PHC string argon2id with today's parameters?
pub fn current(digest: &str) -> bool {
    digest.starts_with("$argon2id$v=19$")
        && digest.contains(&format!("m={M_COST_KIB},t={T_COST},p={P_COST}$"))
}

/// A hash of a random string, computed once. Unknown accounts are verified against it.
// SECURITY: without it, "no such account" returns in microseconds and "wrong password" in
// ~50 ms: the timing would enumerate accounts.
// INVARIANT: unknown emails cost one argon2 verify (test: unknown_and_known_emails_take_the_same_path)
static DUMMY_DIGEST: LazyLock<String> = LazyLock::new(|| {
    let mut bytes = [0u8; 24];
    rand::fill(&mut bytes);
    let random: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    hasher()
        .ok()
        .and_then(|h| h.hash_password(random.as_bytes()).ok())
        .map(|h| h.to_string())
        .unwrap_or_default()
});

/// Checks `password` for `user_id` (or for nobody): always exactly one argon2 verify.
pub async fn check(
    db: &impl ConnectionTrait,
    user_id: Option<i64>,
    password: &str,
) -> Result<Checked, AppError> {
    // WHY: the lookup runs for unknown accounts too (id 0 matches nothing), so both paths cost
    // the same queries as well as the same argon2 verify.
    let stored = Entity::find_by_id(user_id.unwrap_or(0))
        .one(db)
        .await?
        .map(|c| c.digest);
    let real = stored.is_some();
    let digest = stored.unwrap_or_else(|| DUMMY_DIGEST.clone());
    let checked = verify(password.to_owned(), digest).await?;
    Ok(Checked {
        ok: checked.ok && real,
        needs_rehash: checked.needs_rehash && real,
    })
}

/// Sets (or replaces) the account's password hash.
pub async fn set(
    db: &impl ConnectionTrait,
    user_id: i64,
    digest: String,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    Entity::insert(ActiveModel {
        user_id: Set(user_id),
        digest: Set(digest),
        changed_at: Set(now),
    })
    .on_conflict(
        OnConflict::column(Column::UserId)
            .update_columns([Column::Digest, Column::ChangedAt])
            .to_owned(),
    )
    .exec_without_returning(db)
    .await?;
    Ok(())
}

/// Replaces the hash without touching `changed_at` (a parameter upgrade isn't a change).
pub async fn rehash(
    db: &impl ConnectionTrait,
    user_id: i64,
    digest: String,
) -> Result<(), AppError> {
    Entity::update_many()
        .col_expr(Column::Digest, sea_orm::sea_query::Expr::value(digest))
        .filter(Column::UserId.eq(user_id))
        .exec(db)
        .await?;
    Ok(())
}

/// Deletes the account's password (an unverified account claimed by its address's owner:
/// `auth::claim_unverified_account`, wired at its anchor).
pub async fn remove(db: &impl ConnectionTrait, user_id: i64) -> Result<(), AppError> {
    Entity::delete_many()
        .filter(Column::UserId.eq(user_id))
        .exec(db)
        .await?;
    Ok(())
}

/// When the password was last changed, if the account has one (the account page).
pub async fn changed_at(
    db: &impl ConnectionTrait,
    user_id: i64,
) -> Result<Option<DateTime<Utc>>, AppError> {
    Ok(Entity::find_by_id(user_id)
        .one(db)
        .await?
        .map(|c| c.changed_at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hashes_use_argon2id_with_owasp_parameters() {
        let digest = hash("a long enough passphrase".into()).await.unwrap();
        assert!(
            digest.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "{digest}"
        );
        assert!(current(&digest));
        let ok = verify("a long enough passphrase".into(), digest.clone())
            .await
            .unwrap();
        assert_eq!(
            ok,
            Checked {
                ok: true,
                needs_rehash: false
            }
        );
        let bad = verify("a long enough passphrasE".into(), digest)
            .await
            .unwrap();
        assert!(!bad.ok);
        let garbage = verify("x".into(), "not a hash".into()).await.unwrap();
        assert!(!garbage.ok);
    }

    #[tokio::test]
    async fn old_parameters_verify_and_ask_for_a_rehash() {
        let weak = Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(8 * 1024, 1, 1, None).unwrap(),
        )
        .hash_password(b"an older hash for this one")
        .unwrap()
        .to_string();
        let checked = verify("an older hash for this one".into(), weak)
            .await
            .unwrap();
        assert_eq!(
            checked,
            Checked {
                ok: true,
                needs_rehash: true
            }
        );
    }
}
