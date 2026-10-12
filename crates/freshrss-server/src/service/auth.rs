//! Registration, login and session resolution.
//!
//! Passwords are hashed with argon2id. Sessions are opaque random ids in a cookie, with the row in
//! the `session` table — that indirection is what makes "sign out everywhere" and the account
//! menu's session list possible, which a stateless signed cookie cannot do.

use std::str::FromStr;
use std::sync::OnceLock;

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use rusqlite::Connection;

use crate::config::{AppConfig, RegistrationMode};
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
pub fn register(db: &Db, config: &AppConfig, req: RegisterRequest) -> Result<User> {
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

    db.blocking_with(|conn| {
        let existing = user_dao::count(conn)?;
        match config.registration.mode {
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
        if user_dao::find_by_username(conn, &username)?.is_some() {
            return Err(Error::Conflict("that username is already taken".into()));
        }
        if let Some(addr) = email.as_deref() {
            if user_dao::find_by_email(conn, addr)?.is_some() {
                return Err(Error::Conflict(
                    "that email address is already registered".into(),
                ));
            }
        }

        let is_admin = existing == 0 && config.registration.first_user_is_admin;
        let password_hash = hash_password(&req.password)?;
        let id = user_dao::insert(
            conn,
            &user_dao::NewUser {
                username: &username,
                email: email.as_deref(),
                password_hash: &password_hash,
                is_admin,
            },
        )?;
        user_dao::find_by_id(conn, id)?
            .ok_or_else(|| Error::Internal("user vanished immediately after insert".into()))
    })
}

/// Adapts the domain validators' `&'static str` reasons to the crate error type.
fn validation_error(reason: &'static str) -> Error {
    Error::Validation(reason.to_string())
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
pub fn login(
    db: &Db,
    config: &AppConfig,
    req: LoginRequest,
    client: ClientInfo<'_>,
) -> Result<(User, session_dao::Session)> {
    let identifier = req.username.trim();
    if identifier.is_empty() || req.password.is_empty() {
        return Err(Error::BadCredentials);
    }

    let (user, session) = db.blocking_with(|conn| {
        let user = authenticate(conn, identifier, &req.password)?;
        let ttl = i64::from(config.session.max_age_days) * 86_400;
        let session = session_dao::create(conn, user.id, ttl, client.ip, client.user_agent)?;
        user_dao::touch_last_seen(conn, user.id)?;

        // Upgrade a hash stored under older parameters while we have the plaintext anyway. This is
        // the only moment it is possible, so skipping it would leave the hash weak forever.
        if needs_rehash(&user.password_hash) {
            let fresh = hash_password(&req.password)?;
            user_dao::update_password(conn, user.id, &fresh)?;
        }
        Ok((user, session))
    })?;

    Ok((user, session))
}

/// The shared credential check, so `login` and any future token-based path agree.
fn authenticate(conn: &Connection, identifier: &str, password: &str) -> Result<User> {
    // The username may be given as either the username or the email, because people do not
    // remember which one they signed up with.
    let user = match user_dao::find_by_username(conn, identifier)? {
        Some(user) => Some(user),
        None => user_dao::find_by_email(conn, identifier)?,
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

pub fn logout(db: &Db, session_id: &str) -> Result<()> {
    db.blocking_with(|conn| {
        session_dao::revoke(conn, session_id)?;
        Ok(())
    })
}

/// Resolves a session cookie to a user, or `None` when absent, expired or revoked.
pub fn resolve_session(db: &Db, session_id: &str) -> Result<Option<User>> {
    db.blocking_with(|conn| {
        let Some(session) = session_dao::find_valid(conn, session_id)? else {
            return Ok(None);
        };
        Ok(user_dao::find_by_id(conn, session.id_user)?)
    })
}

/// Changes a password and signs out every other session.
///
/// Revoking the others is what makes "I think my password leaked" actionable: changing it alone
/// leaves any stolen session cookie working until it expires.
pub fn change_password(
    db: &Db,
    id_user: i64,
    current: &str,
    next: &str,
    keep_session: Option<&str>,
) -> Result<usize> {
    User::validate_password(next).map_err(validation_error)?;

    db.blocking_with(|conn| {
        let user = user_dao::find_by_id(conn, id_user)?
            .ok_or_else(|| Error::NotFound(format!("user {id_user}")))?;
        if !verify_password(current, &user.password_hash) {
            return Err(Error::BadCredentials);
        }
        let hash = hash_password(next)?;
        user_dao::update_password(conn, id_user, &hash)?;
        let revoked = match keep_session {
            Some(sid) => session_dao::revoke_others(conn, id_user, sid)?,
            None => session_dao::revoke_for_user(conn, id_user)?,
        };
        Ok(revoked)
    })
}
