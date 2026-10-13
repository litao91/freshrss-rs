//! Registration, login and session resolution.
//!
//! Passwords are hashed with argon2id. Sessions are opaque random ids in a cookie, with the row in
//! the `session` table — that indirection is what makes "sign out everywhere" and the account
//! menu's session list possible, which a stateless signed cookie cannot do.

use std::str::FromStr;
use std::sync::OnceLock;

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use sqlx::SqliteConnection;

use crate::config::{AppConfig, RegistrationConfig, RegistrationMode};
use crate::db::dao::{session as session_dao, user as user_dao};
use crate::db::Db;
use crate::domain::user::User;
use crate::error::{Error, Result};

/// Argon2id parameters.
///
/// These are the OWASP-recommended defaults that `Argon2::default()` gives us (19 MiB, 2
/// iterations, parallelism 1). They are deliberately spelled out here rather than left implicit,
/// because [`needs_rehash`] compares against them to decide whether a stored hash should be
/// upgraded on the next successful login.
const PHC_PREFIX: &str = "$argon2id$v=19$m=19456,t=2,p=1$";

pub fn hash_password(password: &str) -> Result<String> {
    // `hash_password` generates its own random salt (argon2's `getrandom` feature), so there is no
    // salt handling here to get wrong.
    let hash: PasswordHash = Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|err| {
            tracing::error!(%err, "argon2 hashing failed");
            Error::PasswordHash
        })?;
    Ok(hash.to_string())
}

pub fn verify_password(password: &str, phc: &str) -> bool {
    // A malformed stored hash must be a failed login, not a 500: the user sees "invalid username
    // or password" and an administrator can reset it out of band.
    let Ok(parsed) = PasswordHash::from_str(phc) else {
        tracing::warn!("stored password hash is not a valid PHC string");
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// True when a stored hash was produced with parameters other than the current ones, so it should
/// be re-hashed on the next successful login.
///
/// Comparing the PHC prefix covers the algorithm, version and parameters in one shot; that is
/// enough to catch both a legacy import and a future parameter bump.
pub fn needs_rehash(phc: &str) -> bool {
    !phc.starts_with(PHC_PREFIX)
}

/// Everything the login form posts.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    #[serde(default)]
    pub email: Option<String>,
    pub password: String,
}

/// Creates an account.
///
/// The first account becomes an administrator when `registration.first_user_is_admin` is set, which
/// is how a fresh install bootstraps itself without a separate setup flow.
pub async fn register(db: &Db, config: &AppConfig, req: RegisterRequest) -> Result<User> {
    let username = req.username.trim().to_string();
    User::validate_username(&username).map_err(validation_error)?;
    User::validate_password(&req.password).map_err(validation_error)?;

    let email = req
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string);
    User::validate_email(email.as_deref()).map_err(validation_error)?;

    let registration: RegistrationConfig = config.registration.clone();
    db.with(move |mut conn| async move {
        let existing = user_dao::count(&mut conn).await?;
        match registration.mode {
            RegistrationMode::Closed => return Err(Error::RegistrationClosed),
            RegistrationMode::FirstUserOnly if existing > 0 => {
                return Err(Error::RegistrationClosed);
            }
            RegistrationMode::Open | RegistrationMode::FirstUserOnly => {}
        }

        // Checked before hashing so a taken username does not cost an argon2 run — that would let
        // an unauthenticated caller make the server do expensive work per request.
        //
        // This does reveal whether a username exists, which is a deliberate trade: a registration
        // form that says "try again" with no reason is unusable. The login path, where enumeration
        // actually matters, is timing-equalized and says only "invalid credentials".
        if user_dao::find_by_username(&mut conn, &username)
            .await?
            .is_some()
        {
            return Err(Error::Conflict("that username is already taken".into()));
        }
        if let Some(addr) = email.as_deref() {
            if user_dao::find_by_email(&mut conn, addr).await?.is_some() {
                return Err(Error::Conflict(
                    "that email address is already registered".into(),
                ));
            }
        }

        let is_admin = existing == 0 && registration.first_user_is_admin;
        let password_hash = hash_password(&req.password)?;
        let id = user_dao::insert(
            &mut conn,
            &user_dao::NewUser {
                username: &username,
                email: email.as_deref(),
                password_hash: &password_hash,
                is_admin,
            },
        )
        .await?;
        user_dao::find_by_id(&mut conn, id)
            .await?
            .ok_or_else(|| Error::Internal("user vanished immediately after insert".into()))
    })
    .await
}

/// Adapts the domain validators' `&'static str` reasons to the crate error type.
fn validation_error(reason: &'static str) -> Error {
    Error::Validation(reason.to_string())
}

/// What the registration policy allows right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistrationPolicy {
    pub mode: RegistrationMode,
    /// Whether a `POST /auth/register` would get past the policy check this instant.
    pub can_register: bool,
}

/// Answers whether a visitor may register, before they fill in a form.
///
/// The login page asks this so a server configured `registration.mode = "closed"` never offers a
/// sign-up form whose submit is guaranteed to be refused; the endpoint stays the authority, and
/// this only spares the visitor the round trip. The decision is the same one [`register`] makes,
/// including `first-user-only` closing once an account exists.
pub async fn registration_policy(db: &Db, config: &AppConfig) -> Result<RegistrationPolicy> {
    let mode = config.registration.mode;
    let existing = db
        .with(move |mut conn| async move { user_dao::count(&mut conn).await })
        .await?;
    let can_register = match mode {
        RegistrationMode::Open => true,
        RegistrationMode::FirstUserOnly => existing == 0,
        RegistrationMode::Closed => false,
    };
    Ok(RegistrationPolicy { mode, can_register })
}

/// Creates an account without consulting the registration policy.
///
/// This is what the `create-user` subcommand calls: an operator at a shell has already
/// authenticated, in the strongest sense available, and a server configured
/// `registration.mode = "closed"` still needs a way to gain its second user.
///
/// # Errors
///
/// The same validation and conflict errors as [`register`].
pub async fn create_user(
    db: &Db,
    username: &str,
    email: Option<&str>,
    password: &str,
    is_admin: bool,
) -> Result<User> {
    let username = username.trim().to_string();
    User::validate_username(&username).map_err(validation_error)?;
    User::validate_password(password).map_err(validation_error)?;
    let email = email
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string);
    User::validate_email(email.as_deref()).map_err(validation_error)?;

    db.with(move |mut conn| async move {
        if user_dao::find_by_username(&mut conn, &username)
            .await?
            .is_some()
        {
            return Err(Error::Conflict("that username is already taken".into()));
        }
        if let Some(addr) = email.as_deref() {
            if user_dao::find_by_email(&mut conn, addr).await?.is_some() {
                return Err(Error::Conflict(
                    "that email address is already registered".into(),
                ));
            }
        }
        let password_hash = hash_password(password)?;
        let id = user_dao::insert(
            &mut conn,
            &user_dao::NewUser {
                username: &username,
                email: email.as_deref(),
                password_hash: &password_hash,
                is_admin,
            },
        )
        .await?;
        user_dao::find_by_id(&mut conn, id)
            .await?
            .ok_or_else(|| Error::Internal("user vanished immediately after insert".into()))
    })
    .await
}

/// Everything the login form posts.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// Context recorded against the session, for the account menu's device list.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientInfo<'a> {
    pub ip: Option<&'a str>,
    pub user_agent: Option<&'a str>,
}

/// Verifies credentials and opens a session.
///
/// The username may be given as either the username or the email, because people do not remember
/// which one they signed up with.
pub async fn login(
    db: &Db,
    config: &AppConfig,
    req: LoginRequest,
    client: ClientInfo<'_>,
) -> Result<(User, session_dao::Session)> {
    let identifier = req.username.trim();
    if identifier.is_empty() || req.password.is_empty() {
        return Err(Error::BadCredentials);
    }

    let ttl = i64::from(config.session.max_age_days) * 86_400;
    let ip = client.ip.map(str::to_owned);
    let user_agent = client.user_agent.map(str::to_owned);
    db.with(move |mut conn| async move {
        let user = authenticate(&mut conn, identifier, &req.password).await?;
        let session = session_dao::create(
            &mut conn,
            user.id,
            ttl,
            ip.as_deref(),
            user_agent.as_deref(),
        )
        .await?;
        user_dao::touch_last_seen(&mut conn, user.id).await?;

        // Upgrade a hash stored under older parameters while we have the plaintext anyway. This is
        // the only moment it is possible, so skipping it would leave the hash weak forever.
        if needs_rehash(&user.password_hash) {
            let fresh = hash_password(&req.password)?;
            user_dao::update_password(&mut conn, user.id, &fresh).await?;
        }
        Ok((user, session))
    })
    .await
}

/// The shared credential check, so `login` and any future token-based path agree.
async fn authenticate(
    conn: &mut SqliteConnection,
    identifier: &str,
    password: &str,
) -> Result<User> {
    // The username may be given as either the username or the email, because people do not
    // remember which one they signed up with.
    let user = match user_dao::find_by_username(conn, identifier).await? {
        Some(user) => Some(user),
        None => user_dao::find_by_email(conn, identifier).await?,
    };

    let Some(user) = user else {
        // Verify against a real hash of an unguessable password so a miss costs the same time as a
        // hit. Without this the response time tells an attacker which usernames exist.
        let _ = verify_password(password, dummy_hash());
        return Err(Error::BadCredentials);
    };
    if !user.enabled {
        return Err(Error::AccountDisabled);
    }
    if !verify_password(password, &user.password_hash) {
        return Err(Error::BadCredentials);
    }
    Ok(user)
}

/// A valid argon2id hash of a random string, computed once per process.
///
/// It has to be a *parseable* PHC string with the real parameters, or [`verify_password`] would
/// bail out before doing any work and the timing equalization would be worthless.
fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY
        .get_or_init(|| {
            let unguessable = uuid::Uuid::new_v4().to_string();
            hash_password(&unguessable).expect("argon2 hashing a generated password cannot fail")
        })
        .as_str()
}

pub async fn logout(db: &Db, session_id: &str) -> Result<()> {
    db.with(move |mut conn| async move {
        session_dao::revoke(&mut conn, session_id).await?;
        Ok(())
    })
    .await
}

/// Resolves a session cookie to a user, or `None` when absent, expired or revoked.
pub async fn resolve_session(db: &Db, session_id: &str) -> Result<Option<User>> {
    db.with(move |mut conn| async move {
        let Some(session) = session_dao::find_valid(&mut conn, session_id).await? else {
            return Ok(None);
        };
        user_dao::find_by_id(&mut conn, session.id_user).await
    })
    .await
}

/// Changes a password and signs out every other session.
///
/// Revoking the others is what makes "I think my password leaked" actionable: changing it alone
/// leaves any stolen session cookie working until it expires.
pub async fn change_password(
    db: &Db,
    id_user: i64,
    current: &str,
    next: &str,
    keep_session: Option<&str>,
) -> Result<usize> {
    User::validate_password(next).map_err(validation_error)?;
    let current = current.to_owned();
    let next = next.to_owned();
    let keep_session = keep_session.map(str::to_owned);

    db.with(move |mut conn| async move {
        let user = user_dao::find_by_id(&mut conn, id_user)
            .await?
            .ok_or_else(|| Error::NotFound(format!("user {id_user}")))?;
        if !verify_password(&current, &user.password_hash) {
            return Err(Error::BadCredentials);
        }
        // Hashed only once the old password has been proved, so a wrong `current` costs no argon2
        // run — the same reason `register` checks for a taken username before hashing.
        let hash = hash_password(&next)?;
        user_dao::update_password(&mut conn, id_user, &hash).await?;
        let revoked = match keep_session.as_deref() {
            Some(sid) => session_dao::revoke_others(&mut conn, id_user, sid).await?,
            None => session_dao::revoke_for_user(&mut conn, id_user).await?,
        };
        Ok(revoked)
    })
    .await
}
