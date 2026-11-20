//! The kit's SQL. Every write that a limit bounds takes a row lock first, so concurrent
//! requests at the edge are serialized and exactly the limit is admitted. Every change
//! that matters writes its security event in the same transaction.

use std::time::Duration;

use serde::Serialize;
use sqlx::{PgExecutor, PgPool, Row as _};
use uuid::Uuid;

use crate::domain::{PlanLimits, Role, admits};
use crate::error::AccountsError;

/// Seconds as a PostgreSQL interval argument.
fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

const ISO: &str = r#"'YYYY-MM-DD"T"HH24:MI:SS"Z"'"#;

/// Appends a security event.
pub async fn event(
    db: impl PgExecutor<'_>,
    kind: &str,
    user: Option<Uuid>,
    workspace: Option<Uuid>,
) -> Result<(), AccountsError> {
    sqlx::query(
        "INSERT INTO account_events (id, kind, user_id, workspace_id) VALUES ($1, $2, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(kind)
    .bind(user)
    .bind(workspace)
    .execute(db)
    .await?;
    Ok(())
}

/// Stores invite digests valid for `ttl`.
pub async fn create_invites(
    db: &PgPool,
    digests: &[[u8; 32]],
    ttl: Duration,
) -> Result<(), AccountsError> {
    let mut tx = db.begin().await?;
    for digest in digests {
        sqlx::query("INSERT INTO invites (digest, expires_at) VALUES ($1, now() + make_interval(secs => $2))")
            .bind(&digest[..])
            .bind(secs(ttl))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// A new account, for [`sign_up`].
pub struct NewUser<'a> {
    /// The invite's digest.
    pub invite: [u8; 32],
    /// The account's id.
    pub id: Uuid,
    /// The normalized email.
    pub email: &'a str,
    /// The password's PHC string.
    pub password_hash: &'a str,
    /// The verification token's digest.
    pub verify: [u8; 32],
    /// How long the verification link lives.
    pub verify_ttl: Duration,
}

/// Claims the invite and creates the unverified account with its verification token, in
/// one transaction: a taken email leaves the invite unclaimed.
pub async fn sign_up(db: &PgPool, user: &NewUser<'_>) -> Result<(), AccountsError> {
    let mut tx = db.begin().await?;
    let claimed = sqlx::query(
        "UPDATE invites SET used_at = now()
          WHERE digest = $1 AND used_at IS NULL AND expires_at > now()",
    )
    .bind(&user.invite[..])
    .execute(&mut *tx)
    .await?;
    if claimed.rows_affected() != 1 {
        return Err(AccountsError::InviteInvalid);
    }
    let inserted = sqlx::query("INSERT INTO users (id, email, password_hash) VALUES ($1, $2, $3)")
        .bind(user.id)
        .bind(user.email)
        .bind(user.password_hash)
        .execute(&mut *tx)
        .await;
    match inserted {
        Err(e) if xpay_db::is_unique_violation(&e) => return Err(AccountsError::EmailTaken),
        other => {
            other?;
        }
    }
    sqlx::query("UPDATE invites SET used_by = $2 WHERE digest = $1")
        .bind(&user.invite[..])
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    insert_email_token(&mut *tx, user.id, "verify", user.verify, user.verify_ttl).await?;
    event(&mut *tx, "user.signed_up", Some(user.id), None).await?;
    tx.commit().await?;
    Ok(())
}

async fn insert_email_token(
    db: impl PgExecutor<'_>,
    user: Uuid,
    purpose: &str,
    digest: [u8; 32],
    ttl: Duration,
) -> Result<(), AccountsError> {
    sqlx::query(
        "INSERT INTO email_tokens (digest, user_id, purpose, expires_at)
         VALUES ($1, $2, $3, now() + make_interval(secs => $4))",
    )
    .bind(&digest[..])
    .bind(user)
    .bind(purpose)
    .bind(secs(ttl))
    .execute(db)
    .await?;
    Ok(())
}

/// Replaces the user's unused tokens of `purpose` with a new one.
pub async fn replace_email_token(
    db: &PgPool,
    user: Uuid,
    purpose: &str,
    digest: [u8; 32],
    ttl: Duration,
) -> Result<(), AccountsError> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM email_tokens WHERE user_id = $1 AND purpose = $2 AND used_at IS NULL")
        .bind(user)
        .bind(purpose)
        .execute(&mut *tx)
        .await?;
    insert_email_token(&mut *tx, user, purpose, digest, ttl).await?;
    tx.commit().await?;
    Ok(())
}

/// Uses a verification token: the account is verified. False for an unknown, used or
/// expired token.
pub async fn verify(db: &PgPool, digest: [u8; 32]) -> Result<bool, AccountsError> {
    let mut tx = db.begin().await?;
    let user: Option<Uuid> = sqlx::query_scalar(
        "UPDATE email_tokens SET used_at = now()
          WHERE digest = $1 AND purpose = 'verify' AND used_at IS NULL AND expires_at > now()
          RETURNING user_id",
    )
    .bind(&digest[..])
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user) = user else {
        return Ok(false);
    };
    sqlx::query("UPDATE users SET verified_at = coalesce(verified_at, now()) WHERE id = $1")
        .bind(user)
        .execute(&mut *tx)
        .await?;
    event(&mut *tx, "user.verified", Some(user), None).await?;
    tx.commit().await?;
    Ok(true)
}

/// Uses a reset token: the password changes, and every session and trusted address of the
/// account ends (a reset may follow a compromise). The account's id, or `None`.
pub async fn reset_password(
    db: &PgPool,
    digest: [u8; 32],
    hash: &str,
) -> Result<Option<Uuid>, AccountsError> {
    let mut tx = db.begin().await?;
    let user: Option<Uuid> = sqlx::query_scalar(
        "UPDATE email_tokens SET used_at = now()
          WHERE digest = $1 AND purpose = 'reset' AND used_at IS NULL AND expires_at > now()
          RETURNING user_id",
    )
    .bind(&digest[..])
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user) = user else {
        return Ok(None);
    };
    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(user)
        .bind(hash)
        .execute(&mut *tx)
        .await?;
    for sql in [
        "DELETE FROM sessions WHERE user_id = $1",
        "DELETE FROM sign_in_trust WHERE user_id = $1",
    ] {
        sqlx::query(sql).bind(user).execute(&mut *tx).await?;
    }
    event(&mut *tx, "password.reset", Some(user), None).await?;
    tx.commit().await?;
    Ok(Some(user))
}

/// An account, as sign-in sees it.
#[derive(Debug, Clone)]
pub struct UserRow {
    /// Its id.
    pub id: Uuid,
    /// Its password's PHC string.
    pub password_hash: String,
    /// Whether its email is verified.
    pub verified: bool,
}

/// The account with this normalized email.
pub async fn user_by_email(db: &PgPool, email: &str) -> Result<Option<UserRow>, AccountsError> {
    let row = sqlx::query(
        "SELECT id, password_hash, verified_at IS NOT NULL AS verified FROM users WHERE email = $1",
    )
    .bind(email)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| UserRow {
        id: r.get("id"),
        password_hash: r.get("password_hash"),
        verified: r.get("verified"),
    }))
}

/// The account's password hash.
pub async fn password_hash(db: &PgPool, user: Uuid) -> Result<Option<String>, AccountsError> {
    Ok(
        sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
            .bind(user)
            .fetch_optional(db)
            .await?,
    )
}

/// True when the pair signed in successfully within `window`.
pub async fn is_trusted(
    db: &PgPool,
    user: Uuid,
    address: &str,
    window: Duration,
) -> Result<bool, AccountsError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sign_in_trust
                         WHERE user_id = $1 AND address = $2
                           AND last_success_at > now() - make_interval(secs => $3))",
    )
    .bind(user)
    .bind(address)
    .bind(secs(window))
    .fetch_one(db)
    .await?)
}

/// A successful sign-in: the pair is trusted, the session exists, the event is written.
pub async fn signed_in(
    db: &PgPool,
    user: Uuid,
    address: &str,
    session: [u8; 32],
    ttl: Duration,
) -> Result<(), AccountsError> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "INSERT INTO sign_in_trust (user_id, address, last_success_at) VALUES ($1, $2, now())
         ON CONFLICT (user_id, address) DO UPDATE SET last_success_at = now()",
    )
    .bind(user)
    .bind(address)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO sessions (digest, user_id, expires_at) VALUES ($1, $2, now() + make_interval(secs => $3))")
        .bind(&session[..])
        .bind(user)
        .bind(secs(ttl))
        .execute(&mut *tx)
        .await?;
    event(&mut *tx, "session.created", Some(user), None).await?;
    tx.commit().await?;
    Ok(())
}

/// The user behind a live session: not expired, used within `idle`. Touches it at most
/// once a minute, so a busy session is not a write per request.
pub async fn session_user(
    db: &PgPool,
    digest: [u8; 32],
    idle: Duration,
) -> Result<Option<Uuid>, AccountsError> {
    Ok(sqlx::query_scalar(
        "UPDATE sessions
            SET last_seen_at = CASE WHEN last_seen_at < now() - interval '60 seconds'
                                    THEN now() ELSE last_seen_at END
          WHERE digest = $1 AND expires_at > now()
            AND last_seen_at > now() - make_interval(secs => $2)
          RETURNING user_id",
    )
    .bind(&digest[..])
    .bind(secs(idle))
    .fetch_optional(db)
    .await?)
}

/// Ends a session.
pub async fn delete_session(db: &PgPool, digest: [u8; 32]) -> Result<Option<Uuid>, AccountsError> {
    Ok(
        sqlx::query_scalar("DELETE FROM sessions WHERE digest = $1 RETURNING user_id")
            .bind(&digest[..])
            .fetch_optional(db)
            .await?,
    )
}

/// The token and workspace behind an API token's digest; touches it at most once a minute.
pub async fn token_workspace(
    db: &PgPool,
    digest: [u8; 32],
) -> Result<Option<(Uuid, Uuid)>, AccountsError> {
    let row = sqlx::query(
        "UPDATE api_tokens
            SET last_used_at = CASE WHEN last_used_at IS NULL OR last_used_at < now() - interval '60 seconds'
                                    THEN now() ELSE last_used_at END
          WHERE digest = $1
          RETURNING id, workspace_id",
    )
    .bind(&digest[..])
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| (r.get("id"), r.get("workspace_id"))))
}

/// The user's role in the workspace.
pub async fn role_in(
    db: &PgPool,
    workspace: Uuid,
    user: Uuid,
) -> Result<Option<Role>, AccountsError> {
    let role: Option<String> =
        sqlx::query_scalar("SELECT role FROM members WHERE workspace_id = $1 AND user_id = $2")
            .bind(workspace)
            .bind(user)
            .fetch_optional(db)
            .await?;
    Ok(role.as_deref().and_then(Role::parse))
}

/// One of a user's workspaces.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceRow {
    /// Its id.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// Its plan.
    pub plan: String,
    /// The caller's role, when the caller is a member.
    pub role: Option<Role>,
}

/// The account's email, verification and workspaces.
pub async fn me(
    db: &PgPool,
    user: Uuid,
) -> Result<Option<(String, bool, Vec<WorkspaceRow>)>, AccountsError> {
    let Some(row) =
        sqlx::query("SELECT email, verified_at IS NOT NULL AS verified FROM users WHERE id = $1")
            .bind(user)
            .fetch_optional(db)
            .await?
    else {
        return Ok(None);
    };
    let workspaces = sqlx::query(
        "SELECT w.id, w.name, w.plan, m.role FROM members m JOIN workspaces w ON w.id = m.workspace_id
          WHERE m.user_id = $1 ORDER BY w.created_at, w.id",
    )
    .bind(user)
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|r| WorkspaceRow {
        id: r.get("id"),
        name: r.get("name"),
        plan: r.get("plan"),
        role: Role::parse(r.get::<String, _>("role").as_str()),
    })
    .collect();
    Ok(Some((row.get("email"), row.get("verified"), workspaces)))
}

/// Creates a workspace owned by `user`, within the plan's owned-workspaces limit.
pub async fn create_workspace(
    db: &PgPool,
    user: Uuid,
    name: &str,
    plan: PlanLimits,
) -> Result<Uuid, AccountsError> {
    let mut tx = db.begin().await?;
    // Serializes this user's concurrent creates at the limit.
    sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR UPDATE")
        .bind(user)
        .execute(&mut *tx)
        .await?;
    let owned: i64 =
        sqlx::query_scalar("SELECT count(*) FROM members WHERE user_id = $1 AND role = 'owner'")
            .bind(user)
            .fetch_one(&mut *tx)
            .await?;
    if !admits(owned, plan.max_owned_workspaces) {
        return Err(AccountsError::PlanLimit("owned_workspaces"));
    }
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, name, created_by) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(name)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO members (workspace_id, user_id, role) VALUES ($1, $2, 'owner')")
        .bind(id)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    event(&mut *tx, "workspace.created", Some(user), Some(id)).await?;
    tx.commit().await?;
    Ok(id)
}

/// The workspace's name and plan.
pub async fn workspace(db: &PgPool, id: Uuid) -> Result<Option<(String, String)>, AccountsError> {
    let row = sqlx::query("SELECT name, plan FROM workspaces WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| (r.get("name"), r.get("plan"))))
}

/// Deletes a workspace and everything it owns (members and tokens cascade).
pub async fn delete_workspace(db: &PgPool, id: Uuid, actor: Uuid) -> Result<(), AccountsError> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    event(&mut *tx, "workspace.deleted", Some(actor), Some(id)).await?;
    tx.commit().await?;
    Ok(())
}

/// One member.
#[derive(Debug, Clone, Serialize)]
pub struct MemberRow {
    /// The account.
    pub user_id: Uuid,
    /// Its email.
    pub email: String,
    /// Its role.
    pub role: Option<Role>,
}

/// The workspace's members.
pub async fn members(db: &PgPool, workspace: Uuid) -> Result<Vec<MemberRow>, AccountsError> {
    Ok(sqlx::query(
        "SELECT m.user_id, u.email, m.role FROM members m JOIN users u ON u.id = m.user_id
          WHERE m.workspace_id = $1 ORDER BY m.added_at, m.user_id",
    )
    .bind(workspace)
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|r| MemberRow {
        user_id: r.get("user_id"),
        email: r.get("email"),
        role: Role::parse(r.get::<String, _>("role").as_str()),
    })
    .collect())
}

/// Adds a verified account to the workspace, within the plan's member limit.
pub async fn add_member(
    db: &PgPool,
    workspace: Uuid,
    email: &str,
    role: Role,
    actor: Uuid,
    plan: PlanLimits,
) -> Result<Uuid, AccountsError> {
    let mut tx = db.begin().await?;
    sqlx::query("SELECT 1 FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace)
        .execute(&mut *tx)
        .await?;
    let user: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM users WHERE email = $1 AND verified_at IS NOT NULL")
            .bind(email)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(user) = user else {
        return Err(AccountsError::NotFound);
    };
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM members WHERE workspace_id = $1")
        .bind(workspace)
        .fetch_one(&mut *tx)
        .await?;
    if !admits(count, plan.max_members) {
        return Err(AccountsError::PlanLimit("members"));
    }
    let inserted =
        sqlx::query("INSERT INTO members (workspace_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(workspace)
            .bind(user)
            .bind(role.as_str())
            .execute(&mut *tx)
            .await;
    match inserted {
        Err(e) if xpay_db::is_unique_violation(&e) => {
            return Err(AccountsError::Conflict("already_member"));
        }
        other => {
            other?;
        }
    }
    event(&mut *tx, "member.added", Some(actor), Some(workspace)).await?;
    tx.commit().await?;
    Ok(user)
}

/// Removes a member. Refuses to remove the last owner, and an admin removing an owner.
pub async fn remove_member(
    db: &PgPool,
    workspace: Uuid,
    user: Uuid,
    actor: Uuid,
    actor_role: Role,
) -> Result<(), AccountsError> {
    let mut tx = db.begin().await?;
    sqlx::query("SELECT 1 FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace)
        .execute(&mut *tx)
        .await?;
    let role: Option<String> =
        sqlx::query_scalar("SELECT role FROM members WHERE workspace_id = $1 AND user_id = $2")
            .bind(workspace)
            .bind(user)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(role) = role.as_deref().and_then(Role::parse) else {
        return Err(AccountsError::NotFound);
    };
    if role == Role::Owner {
        if actor_role != Role::Owner {
            return Err(AccountsError::Forbidden);
        }
        let owners: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM members WHERE workspace_id = $1 AND role = 'owner'",
        )
        .bind(workspace)
        .fetch_one(&mut *tx)
        .await?;
        if owners <= 1 {
            return Err(AccountsError::Conflict("last_owner"));
        }
    }
    sqlx::query("DELETE FROM members WHERE workspace_id = $1 AND user_id = $2")
        .bind(workspace)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    event(&mut *tx, "member.removed", Some(actor), Some(workspace)).await?;
    tx.commit().await?;
    Ok(())
}

/// One API token, without its secret.
#[derive(Debug, Clone, Serialize)]
pub struct TokenRow {
    /// Its id.
    pub id: Uuid,
    /// Its display prefix.
    pub prefix: String,
    /// Its name.
    pub name: String,
    /// When it was made (UTC, RFC 3339).
    pub created_at: String,
    /// When it was last used, to the minute.
    pub last_used_at: Option<String>,
}

/// The workspace's tokens.
pub async fn tokens(db: &PgPool, workspace: Uuid) -> Result<Vec<TokenRow>, AccountsError> {
    let sql = format!(
        "SELECT id, prefix, name, to_char(created_at AT TIME ZONE 'UTC', {ISO}) AS created_at,
                to_char(last_used_at AT TIME ZONE 'UTC', {ISO}) AS last_used_at
           FROM api_tokens WHERE workspace_id = $1 ORDER BY created_at, id"
    );
    Ok(sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(workspace)
        .fetch_all(db)
        .await?
        .into_iter()
        .map(|r| TokenRow {
            id: r.get("id"),
            prefix: r.get("prefix"),
            name: r.get("name"),
            created_at: r.get("created_at"),
            last_used_at: r.get("last_used_at"),
        })
        .collect())
}

/// A new API token, for [`create_token`].
pub struct NewToken<'a> {
    /// The workspace.
    pub workspace: Uuid,
    /// Who made it.
    pub actor: Uuid,
    /// Its name.
    pub name: &'a str,
    /// Its display prefix.
    pub prefix: &'a str,
    /// Its digest.
    pub digest: [u8; 32],
}

/// Stores a token, within the plan's token limit.
pub async fn create_token(
    db: &PgPool,
    token: &NewToken<'_>,
    plan: PlanLimits,
) -> Result<Uuid, AccountsError> {
    let mut tx = db.begin().await?;
    sqlx::query("SELECT 1 FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(token.workspace)
        .execute(&mut *tx)
        .await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_tokens WHERE workspace_id = $1")
        .bind(token.workspace)
        .fetch_one(&mut *tx)
        .await?;
    if !admits(count, plan.max_tokens) {
        return Err(AccountsError::PlanLimit("tokens"));
    }
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO api_tokens (id, workspace_id, prefix, digest, name, created_by) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(token.workspace)
    .bind(token.prefix)
    .bind(&token.digest[..])
    .bind(token.name)
    .bind(token.actor)
    .execute(&mut *tx)
    .await?;
    event(
        &mut *tx,
        "token.created",
        Some(token.actor),
        Some(token.workspace),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Deletes a token of this workspace (another workspace's token is not found).
pub async fn delete_token(
    db: &PgPool,
    workspace: Uuid,
    id: Uuid,
    actor: Uuid,
) -> Result<(), AccountsError> {
    let mut tx = db.begin().await?;
    let deleted = sqlx::query("DELETE FROM api_tokens WHERE id = $1 AND workspace_id = $2")
        .bind(id)
        .bind(workspace)
        .execute(&mut *tx)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(AccountsError::NotFound);
    }
    event(&mut *tx, "token.deleted", Some(actor), Some(workspace)).await?;
    tx.commit().await?;
    Ok(())
}

/// Deletes the account and every row the kit holds for it. Workspaces it alone belongs to
/// go with it. One it solely owns that has other members is refused (409
/// `transfer_ownership_first`): deleting an account never deletes other people's work.
/// The security log keeps the account's id, which then resolves to nobody.
pub async fn delete_account(db: &PgPool, user: Uuid) -> Result<(), AccountsError> {
    let mut tx = db.begin().await?;
    sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR UPDATE")
        .bind(user)
        .execute(&mut *tx)
        .await?;
    let blocked: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM members m
              WHERE m.user_id = $1 AND m.role = 'owner'
                AND NOT EXISTS (SELECT 1 FROM members o WHERE o.workspace_id = m.workspace_id
                                   AND o.role = 'owner' AND o.user_id <> $1)
                AND EXISTS (SELECT 1 FROM members x WHERE x.workspace_id = m.workspace_id AND x.user_id <> $1))",
    )
    .bind(user)
    .fetch_one(&mut *tx)
    .await?;
    if blocked {
        return Err(AccountsError::Conflict("transfer_ownership_first"));
    }
    let alone: Vec<Uuid> = sqlx::query_scalar(
        "DELETE FROM workspaces w
          WHERE EXISTS (SELECT 1 FROM members m WHERE m.workspace_id = w.id AND m.user_id = $1)
            AND NOT EXISTS (SELECT 1 FROM members x WHERE x.workspace_id = w.id AND x.user_id <> $1)
          RETURNING w.id",
    )
    .bind(user)
    .fetch_all(&mut *tx)
    .await?;
    for workspace in alone {
        event(&mut *tx, "workspace.deleted", Some(user), Some(workspace)).await?;
    }
    // Sessions, memberships, links and trusted addresses cascade; invites, workspaces and
    // tokens it made keep no reference to it (ON DELETE SET NULL).
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user)
        .execute(&mut *tx)
        .await?;
    event(&mut *tx, "user.deleted", Some(user), None).await?;
    tx.commit().await?;
    Ok(())
}
