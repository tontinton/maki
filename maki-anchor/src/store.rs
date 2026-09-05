use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use argon2::{
    Argon2, PasswordHash, PasswordHasher as _, PasswordVerifier as _, password_hash::SaltString,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

/// Rights attached to a share link or a user grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Viewer,
    Controller,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Controller => "controller",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "view" | "viewer" => Some(Self::Viewer),
            "control" | "controller" => Some(Self::Controller),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MintTokens {
    Any,
    #[default]
    User,
    Admin,
}

impl MintTokens {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "any" | "anonymous" => Some(Self::Any),
            "user" | "users" | "authenticated" => Some(Self::User),
            "admin" | "admins" => Some(Self::Admin),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::User => "user",
            Self::Admin => "admin",
        }
    }
}

/// Instance names must be safe to mint links for and echo in the dashboard.
pub fn valid_instance_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    Exists,
    #[error("hash: {0}")]
    Hash(String),
}

pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    hex(&digest)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Rows past expiry by more than this are pruned when new links are minted.
const LINK_GRACE_SECS: i64 = 24 * 60 * 60;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(serde::Serialize)]
pub struct InstanceRow {
    pub id: i64,
    pub name: String,
    pub last_seen: i64,
}

#[derive(Debug, serde::Serialize, Clone)]
pub struct UserRow {
    pub id: i64,
    pub oidc_sub: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub is_admin: bool,
}

#[derive(serde::Serialize)]
pub struct SessionRow {
    pub instance_id: i64,
    pub external_id: String,
    pub title: String,
    pub model: String,
    pub cwd: String,
    pub status: String,
    pub cost_cents: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub context_window: i64,
    pub updated_at: i64,
}

pub struct LiveLink {
    pub token: String,
    pub token_hash: String,
}

/// A live link as the management pages show it. `token` is None for links
/// minted before plaintext storage existed.
#[derive(Debug)]
pub struct LinkView {
    pub token: Option<String>,
    pub token_hash: String,
    pub instance_id: i64,
    pub instance_name: String,
    pub external_session_id: Option<String>,
    pub rights: String,
    pub expires_at: i64,
}

#[derive(Debug)]
pub struct LinkRow {
    pub token_hash: String,
    pub instance_id: i64,
    pub external_session_id: Option<String>,
    pub rights: String,
}

impl Store {
    pub fn open(path: &Path) -> Result<Arc<Self>, StoreError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS instances (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL UNIQUE,
                 registration_token_hash TEXT NOT NULL,
                 last_seen INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS sessions (
                 instance_id INTEGER NOT NULL REFERENCES instances(id) ON DELETE CASCADE,
                 external_id TEXT NOT NULL,
                 title TEXT NOT NULL,
                 model TEXT NOT NULL,
                 cwd TEXT NOT NULL,
                 status TEXT NOT NULL,
                 cost_cents INTEGER NOT NULL DEFAULT 0,
                 tokens_in INTEGER NOT NULL DEFAULT 0,
                 tokens_out INTEGER NOT NULL DEFAULT 0,
                 context_window INTEGER NOT NULL DEFAULT 0,
                 updated_at INTEGER NOT NULL,
                 PRIMARY KEY (instance_id, external_id)
             );
              CREATE TABLE IF NOT EXISTS links (
                  token_hash TEXT PRIMARY KEY,
                  token_plain TEXT,
                  instance_id INTEGER NOT NULL REFERENCES instances(id) ON DELETE CASCADE,
                  external_session_id TEXT,
                  rights TEXT NOT NULL,
                  expires_at INTEGER NOT NULL,
                  revoked_at INTEGER
              );
              CREATE TABLE IF NOT EXISTS settings (
                  key TEXT PRIMARY KEY,
                  value TEXT NOT NULL
              );
              CREATE TABLE IF NOT EXISTS users (
                  id INTEGER PRIMARY KEY,
                  oidc_sub TEXT NOT NULL UNIQUE,
                  email TEXT,
                  name TEXT,
                  is_admin INTEGER NOT NULL DEFAULT 0,
                  password_hash TEXT,
                  local_username TEXT UNIQUE
              );
             CREATE TABLE IF NOT EXISTS oidc_sessions (
                 cookie TEXT PRIMARY KEY,
                 user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                 expires_at INTEGER NOT NULL
             );
              CREATE TABLE IF NOT EXISTS grants (
                  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                  instance_id INTEGER NOT NULL REFERENCES instances(id) ON DELETE CASCADE,
                  rights TEXT NOT NULL,
                  PRIMARY KEY (user_id, instance_id)
               );
              CREATE INDEX IF NOT EXISTS instances_registration_token
                  ON instances(registration_token_hash);",
        )?;
        // Migrations for local users (add columns if DB was created before this version).
        let _ = conn.execute("ALTER TABLE users ADD COLUMN password_hash TEXT", []);
        let _ = conn.execute("ALTER TABLE users ADD COLUMN local_username TEXT", []);
        // The dashboard re-shows live links, which needs the token it once
        // handed out; links minted before this column only ever appear bare.
        let _ = conn.execute("ALTER TABLE links ADD COLUMN token_plain TEXT", []);
        // Ensure unique index for local_username where not null (older DBs may not have it).
        let _ = conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS users_local_username_unique ON users(local_username) WHERE local_username IS NOT NULL",
            [],
        );
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
        }))
    }

    /// CLI registration: create, or rotate an existing instance's token.
    pub fn register_instance(
        &self,
        name: &str,
        registration_token_hash: &str,
    ) -> Result<i64, StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO instances (name, registration_token_hash, last_seen) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET registration_token_hash = excluded.registration_token_hash,
             last_seen = excluded.last_seen",
            rusqlite::params![name, registration_token_hash, now_unix()],
        )?;
        instance_id_by_name_locked(&conn, name)
    }

    /// Dashboard registration: create only. Rotating a live instance's token
    /// over an anonymous API would hand whoever guesses the name the ability
    /// to displace and impersonate the real tunnel, so the API never rotates.
    pub fn create_instance(
        &self,
        name: &str,
        registration_token_hash: &str,
    ) -> Result<i64, StoreError> {
        let conn = self.conn.lock().unwrap();
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM instances WHERE name = ?1",
            [name],
            |r| r.get(0),
        )?;
        if exists > 0 {
            return Err(StoreError::Exists);
        }
        conn.execute(
            "INSERT INTO instances (name, registration_token_hash, last_seen) VALUES (?1, ?2, ?3)",
            rusqlite::params![name, registration_token_hash, now_unix()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Hashes are indexed; an exact lookup is both constant-time and O(1).
    pub fn instance_by_registration_token(&self, token: &str) -> Result<InstanceRow, StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, name, last_seen FROM instances WHERE registration_token_hash = ?1",
            [hash_token(token)],
            |row| {
                Ok(InstanceRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    last_seen: row.get(2)?,
                })
            },
        )
        .map_err(StoreError::from)
    }

    pub fn touch_instance(&self, id: i64) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE instances SET last_seen = ?1 WHERE id = ?2",
            rusqlite::params![now_unix(), id],
        )?;
        Ok(())
    }

    pub fn upsert_session(&self, session: &SessionRow) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (instance_id, external_id, title, model, cwd, status, cost_cents,
                 tokens_in, tokens_out, context_window, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(instance_id, external_id) DO UPDATE SET
                 title = excluded.title, model = excluded.model, cwd = excluded.cwd,
                 status = excluded.status, cost_cents = excluded.cost_cents,
                 tokens_in = excluded.tokens_in, tokens_out = excluded.tokens_out,
                 context_window = excluded.context_window, updated_at = excluded.updated_at",
            rusqlite::params![
                session.instance_id,
                session.external_id,
                session.title,
                session.model,
                session.cwd,
                session.status,
                session.cost_cents,
                session.tokens_in,
                session.tokens_out,
                session.context_window,
                session.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn sessions_for_instance(&self, instance_id: i64) -> Result<Vec<SessionRow>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT instance_id, external_id, title, model, cwd, status, cost_cents, tokens_in,
                 tokens_out, context_window, updated_at
             FROM sessions WHERE instance_id = ?1 ORDER BY updated_at DESC LIMIT 200",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![instance_id], map_session)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn instance_by_name(&self, name: &str) -> Result<InstanceRow, StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, name, last_seen FROM instances WHERE name = ?1",
            rusqlite::params![name],
            |row| {
                Ok(InstanceRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    last_seen: row.get(2)?,
                })
            },
        )
        .map_err(StoreError::from)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRow>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT instance_id, external_id, title, model, cwd, status, cost_cents, tokens_in,
                 tokens_out, context_window, updated_at
             FROM sessions ORDER BY updated_at DESC LIMIT 500",
        )?;
        let rows = stmt
            .query_map([], map_session)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_instances(&self) -> Result<Vec<InstanceRow>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, name, last_seen FROM instances ORDER BY name")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(InstanceRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    last_seen: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Expired links still in the table are only ever scanned by direct
    /// lookups, so prune them opportunistically when minting.
    pub fn create_link(
        &self,
        token: &str,
        instance_id: i64,
        external_session_id: Option<&str>,
        rights: &str,
        ttl: Duration,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_unix();
        conn.execute(
            "DELETE FROM links WHERE expires_at < ?1",
            [now - LINK_GRACE_SECS],
        )?;
        conn.execute(
            "INSERT INTO links (token_hash, token_plain, instance_id, external_session_id, rights, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                hash_token(token),
                token,
                instance_id,
                external_session_id,
                rights,
                now + ttl.as_secs() as i64,
            ],
        )?;
        Ok(())
    }

    /// The instance's still-live unscoped control link, if any: reconnects
    /// and repeated `/rc` calls reuse it so the shared URL stays stable.
    /// Links minted before plaintext storage cannot be re-shown, and are
    /// skipped rather than resurrected.
    pub fn live_control_link(&self, instance_id: i64) -> Result<Option<LiveLink>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let found = conn.query_row(
            "SELECT token_plain, token_hash FROM links
             WHERE instance_id = ?1 AND external_session_id IS NULL AND rights = 'controller'
               AND revoked_at IS NULL AND expires_at > ?2 AND token_plain IS NOT NULL
             ORDER BY expires_at DESC LIMIT 1",
            rusqlite::params![instance_id, now_unix()],
            |row| {
                Ok(LiveLink {
                    token: row.get(0)?,
                    token_hash: row.get(1)?,
                })
            },
        );
        match found {
            Ok(link) => Ok(Some(link)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Live (unexpired, unrevoked) links with their instance names, newest
    /// expiry first; the sessions home and the links page.
    pub fn list_links(&self) -> Result<Vec<LinkView>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT l.token_plain, l.token_hash, l.instance_id, i.name, l.external_session_id, l.rights, l.expires_at
             FROM links l JOIN instances i ON i.id = l.instance_id
             WHERE l.revoked_at IS NULL AND l.expires_at > ?1
             ORDER BY l.expires_at DESC",
        )?;
        let rows = stmt
            .query_map([now_unix()], |row| {
                Ok(LinkView {
                    token: row.get(0)?,
                    token_hash: row.get(1)?,
                    instance_id: row.get(2)?,
                    instance_name: row.get(3)?,
                    external_session_id: row.get(4)?,
                    rights: row.get(5)?,
                    expires_at: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn link_by_token(&self, token: &str) -> Result<LinkRow, StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT token_hash, instance_id, external_session_id, rights
             FROM links WHERE token_hash = ?1 AND revoked_at IS NULL AND expires_at > ?2",
            rusqlite::params![hash_token(token), now_unix()],
            |row| {
                Ok(LinkRow {
                    token_hash: row.get(0)?,
                    instance_id: row.get(1)?,
                    external_session_id: row.get(2)?,
                    rights: row.get(3)?,
                })
            },
        )
        .map_err(StoreError::from)
    }

    /// Slides a live link's expiry forward, so a control link minted for a
    /// tunnel outlives the tunnel instead of stranding the user mid-session.
    pub fn extend_link(&self, token_hash: &str, ttl: Duration) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE links SET expires_at = ?1 WHERE token_hash = ?2 AND revoked_at IS NULL",
            rusqlite::params![now_unix() + ttl.as_secs() as i64, token_hash],
        )?;
        Ok(())
    }

    /// Upsert a user after OIDC login. The very first user of a fresh
    /// deployment becomes admin so it can self-bootstrap; once any user
    /// exists, logging in never grants admin, so deleting the last admin
    /// cannot hand the role to whoever happens to authenticate next.
    pub fn upsert_user(
        &self,
        oidc_sub: &str,
        email: Option<&str>,
        name: Option<&str>,
    ) -> Result<UserRow, StoreError> {
        let conn = self.conn.lock().unwrap();
        let users = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, i64>(0))?;
        conn.execute(
            "INSERT INTO users (oidc_sub, email, name, is_admin) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(oidc_sub) DO UPDATE SET email = excluded.email, name = excluded.name",
            rusqlite::params![oidc_sub, email, name, i64::from(users == 0)],
        )?;
        Self::user_by_sub_locked(&conn, oidc_sub)
    }

    pub fn user_by_sub(&self, oidc_sub: &str) -> Result<UserRow, StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, oidc_sub, email, name, is_admin FROM users WHERE oidc_sub = ?1",
            [oidc_sub],
            |row| {
                Ok(UserRow {
                    id: row.get(0)?,
                    oidc_sub: row.get(1)?,
                    email: row.get(2)?,
                    name: row.get(3)?,
                    is_admin: row.get::<_, i64>(4)? != 0,
                })
            },
        )
        .map_err(StoreError::from)
    }

    pub fn user_by_id(&self, id: i64) -> Result<UserRow, StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, oidc_sub, email, name, is_admin FROM users WHERE id = ?1",
            [id],
            |row| {
                Ok(UserRow {
                    id: row.get(0)?,
                    oidc_sub: row.get(1)?,
                    email: row.get(2)?,
                    name: row.get(3)?,
                    is_admin: row.get::<_, i64>(4)? != 0,
                })
            },
        )
        .map_err(StoreError::from)
    }

    /// Create an OIDC browser session; the cookie value is the primary key.
    pub fn create_oidc_session(
        &self,
        cookie: &str,
        user_id: i64,
        ttl: Duration,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO oidc_sessions (cookie, user_id, expires_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![cookie, user_id, now_unix() + ttl.as_secs() as i64],
        )?;
        Ok(())
    }

    /// Resolve a cookie to its user, ignoring expired sessions.
    pub fn user_by_cookie(&self, cookie: &str) -> Result<UserRow, StoreError> {
        let user_id = {
            let conn = self.conn.lock().unwrap();
            let (user_id, expires_at) = conn
                .query_row(
                    "SELECT user_id, expires_at FROM oidc_sessions WHERE cookie = ?1",
                    [cookie],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .map_err(StoreError::from)?;
            if expires_at <= now_unix() {
                conn.execute("DELETE FROM oidc_sessions WHERE cookie = ?1", [cookie])?;
                return Err(StoreError::NotFound);
            }
            user_id
        };
        self.user_by_id(user_id)
    }

    pub fn delete_oidc_session(&self, cookie: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM oidc_sessions WHERE cookie = ?1", [cookie])?;
        Ok(())
    }

    pub fn set_grant(
        &self,
        user_id: i64,
        instance_id: i64,
        rights: Role,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO grants (user_id, instance_id, rights) VALUES (?1, ?2, ?3)
             ON CONFLICT(user_id, instance_id) DO UPDATE SET rights = excluded.rights",
            rusqlite::params![user_id, instance_id, rights.as_str()],
        )?;
        Ok(())
    }

    pub fn delete_grant(&self, user_id: i64, instance_id: i64) -> Result<bool, StoreError> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "DELETE FROM grants WHERE user_id = ?1 AND instance_id = ?2",
            rusqlite::params![user_id, instance_id],
        )?;
        Ok(changed > 0)
    }

    /// The user's rights on an instance: the grant if one exists.
    pub fn grant_for(&self, user_id: i64, instance_id: i64) -> Result<Option<Role>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let rights: Option<String> = conn
            .query_row(
                "SELECT rights FROM grants WHERE user_id = ?1 AND instance_id = ?2",
                rusqlite::params![user_id, instance_id],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(rights.as_deref().and_then(Role::parse))
    }

    pub fn list_grants(&self) -> Result<Vec<(i64, String, String)>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT u.id, i.name, g.rights FROM grants g
             JOIN users u ON u.id = g.user_id
             JOIN instances i ON i.id = g.instance_id
             ORDER BY u.id, i.name",
        )?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn revoke_link(&self, token_hash: &str) -> Result<bool, StoreError> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE links SET revoked_at = ?1 WHERE token_hash = ?2 AND revoked_at IS NULL",
            rusqlite::params![now_unix(), token_hash],
        )?;
        Ok(changed > 0)
    }

    pub fn has_users(&self) -> Result<bool, StoreError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        Ok(count > 0)
    }

    /// Whether local (username+password) logins have any chance of working,
    /// which also covers the admin created by first-run setup on an anchor
    /// that never configured `allow_local`.
    pub fn has_local_users(&self) -> Result<bool, StoreError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE password_hash IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Creates the first admin under the same lock that counts users, so two
    /// concurrent setups cannot overwrite each other's password. `None` once
    /// anyone exists: setup is a one-time door.
    pub fn setup_first_admin(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<UserRow>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let users: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
        if users > 0 {
            return Ok(None);
        }
        let oidc_sub = format!("local:{username}");
        let hash = Self::hash_password(password)?;
        conn.execute(
            "INSERT INTO users (oidc_sub, email, name, is_admin, password_hash, local_username)
             VALUES (?1, NULL, NULL, 1, ?2, ?3)",
            rusqlite::params![oidc_sub, hash, username],
        )?;
        Ok(Some(Self::user_by_sub_locked(&conn, &oidc_sub)?))
    }

    pub fn instance_by_id(&self, id: i64) -> Result<Option<InstanceRow>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let found = conn.query_row(
            "SELECT id, name, last_seen FROM instances WHERE id = ?1",
            [id],
            |row| {
                Ok(InstanceRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    last_seen: row.get(2)?,
                })
            },
        );
        match found {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub fn list_users(&self) -> Result<Vec<UserRow>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, oidc_sub, email, name, is_admin FROM users ORDER BY id")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(UserRow {
                    id: row.get(0)?,
                    oidc_sub: row.get(1)?,
                    email: row.get(2)?,
                    name: row.get(3)?,
                    is_admin: row.get::<_, i64>(4)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let res: Result<String, _> =
            conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            });
        match res {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_user(&self, user_id: i64) -> Result<bool, StoreError> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute("DELETE FROM users WHERE id = ?1", [user_id])?;
        Ok(changed > 0)
    }

    pub fn set_user_admin(&self, user_id: i64, is_admin: bool) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE users SET is_admin = ?1 WHERE id = ?2",
            rusqlite::params![i64::from(is_admin), user_id],
        )?;
        Ok(())
    }

    pub fn sessions_for_user(
        &self,
        user_id: i64,
        is_admin: bool,
    ) -> Result<Vec<SessionRow>, StoreError> {
        if is_admin {
            return self.list_sessions();
        }
        let conn = self.conn.lock().unwrap();
        // Sessions visible where user has a grant on the instance
        let mut stmt = conn.prepare(
            "SELECT s.instance_id, s.external_id, s.title, s.model, s.cwd, s.status, s.cost_cents, s.tokens_in, s.tokens_out, s.context_window, s.updated_at
             FROM sessions s JOIN grants g ON g.instance_id = s.instance_id WHERE g.user_id = ?1 ORDER BY s.updated_at DESC LIMIT 500",
        )?;
        let rows = stmt
            .query_map([user_id], map_session)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn instances_for_user(
        &self,
        user_id: i64,
        is_admin: bool,
    ) -> Result<Vec<InstanceRow>, StoreError> {
        if is_admin {
            return self.list_instances();
        }
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT i.id, i.name, i.last_seen FROM instances i JOIN grants g ON g.instance_id = i.id WHERE g.user_id = ?1 ORDER BY i.name",
        )?;
        let rows = stmt
            .query_map([user_id], |row| {
                Ok(InstanceRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    last_seen: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn user_by_sub_locked(conn: &Connection, oidc_sub: &str) -> Result<UserRow, StoreError> {
        Ok(conn.query_row(
            "SELECT id, oidc_sub, email, name, is_admin FROM users WHERE oidc_sub = ?1",
            [oidc_sub],
            |row| {
                Ok(UserRow {
                    id: row.get(0)?,
                    oidc_sub: row.get(1)?,
                    email: row.get(2)?,
                    name: row.get(3)?,
                    is_admin: row.get::<_, i64>(4)? != 0,
                })
            },
        )?)
    }

    fn hash_password(password: &str) -> Result<String, StoreError> {
        let mut salt_bytes = [0u8; 16];
        getrandom::fill(&mut salt_bytes).expect("rng failed");
        let salt =
            SaltString::encode_b64(&salt_bytes).map_err(|e| StoreError::Hash(e.to_string()))?;
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| StoreError::Hash(e.to_string()))
    }

    /// Legacy single-round SHA-256, verified only to be re-hashed on the spot.
    fn legacy_password_hash(username: &str, password: &str) -> String {
        hash_token(&format!("{username}\0{password}"))
    }

    pub fn create_local_user(
        &self,
        username: &str,
        password: &str,
        email: Option<&str>,
        name: Option<&str>,
        is_admin: bool,
    ) -> Result<UserRow, StoreError> {
        let oidc_sub = format!("local:{username}");
        let hash = Self::hash_password(password)?;
        let conn = self.conn.lock().unwrap();
        let users = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, i64>(0))?;
        let is_admin = is_admin || users == 0;
        conn.execute(
            "INSERT INTO users (oidc_sub, email, name, is_admin, password_hash, local_username) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(oidc_sub) DO UPDATE SET email=excluded.email, name=excluded.name, is_admin=excluded.is_admin, password_hash=excluded.password_hash, local_username=excluded.local_username",
            rusqlite::params![oidc_sub, email, name, i64::from(is_admin), hash, username],
        )?;
        Self::user_by_sub_locked(&conn, &oidc_sub)
    }

    pub fn verify_local_user(&self, username: &str, password: &str) -> Result<UserRow, StoreError> {
        let (user, stored) = {
            let conn = self.conn.lock().unwrap();
            let (user, stored): (UserRow, Option<String>) = conn
                .query_row(
                    "SELECT id, oidc_sub, email, name, is_admin, password_hash FROM users WHERE local_username = ?1",
                    rusqlite::params![username],
                    |row| {
                        Ok((
                            UserRow {
                                id: row.get(0)?,
                                oidc_sub: row.get(1)?,
                                email: row.get(2)?,
                                name: row.get(3)?,
                                is_admin: row.get::<_, i64>(4)? != 0,
                            },
                            row.get::<_, Option<String>>(5)?,
                        ))
                    },
                )
                .map_err(StoreError::from)?;
            (user, stored)
        };
        let Some(stored) = stored else {
            return Err(StoreError::NotFound);
        };
        let ok = if stored.starts_with("$argon2") {
            PasswordHash::new(&stored)
                .map(|hash| {
                    Argon2::default()
                        .verify_password(password.as_bytes(), &hash)
                        .is_ok()
                })
                .unwrap_or(false)
        } else {
            stored == Self::legacy_password_hash(username, password)
        };
        if !ok {
            return Err(StoreError::NotFound);
        }
        if !stored.starts_with("$argon2") {
            let hash = Self::hash_password(password)?;
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "UPDATE users SET password_hash = ?1 WHERE id = ?2",
                rusqlite::params![hash, user.id],
            )?;
        }
        Ok(user)
    }

    pub fn local_user_by_username(&self, username: &str) -> Result<UserRow, StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, oidc_sub, email, name, is_admin FROM users WHERE local_username = ?1",
            [username],
            |row| {
                Ok(UserRow {
                    id: row.get(0)?,
                    oidc_sub: row.get(1)?,
                    email: row.get(2)?,
                    name: row.get(3)?,
                    is_admin: row.get::<_, i64>(4)? != 0,
                })
            },
        )
        .map_err(StoreError::from)
    }
}

fn instance_id_by_name_locked(conn: &Connection, name: &str) -> Result<i64, StoreError> {
    Ok(
        conn.query_row("SELECT id FROM instances WHERE name = ?1", [name], |r| {
            r.get(0)
        })?,
    )
}

fn map_session(row: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        instance_id: row.get(0)?,
        external_id: row.get(1)?,
        title: row.get(2)?,
        model: row.get(3)?,
        cwd: row.get(4)?,
        status: row.get(5)?,
        cost_cents: row.get(6)?,
        tokens_in: row.get(7)?,
        tokens_out: row.get(8)?,
        context_window: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_authenticate_instance() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        let token = "reg-token-1";
        let id = store
            .register_instance("host-a", &hash_token(token))
            .unwrap();
        let row = store.instance_by_registration_token(token).unwrap();
        assert_eq!(row.id, id);
        assert_eq!(row.name, "host-a");
        assert!(store.instance_by_registration_token("wrong").is_err());
    }

    #[test]
    fn user_sessions_and_grants() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        // First user bootstraps to admin.
        let user = store
            .upsert_user("sub-1", Some("a@x"), Some("Alice"))
            .unwrap();
        assert!(user.is_admin);
        let second = store.upsert_user("sub-2", None, None).unwrap();
        assert!(!second.is_admin);

        // Cookie sessions round-trip and expire.
        store
            .create_oidc_session("cookie-1", user.id, Duration::from_secs(60))
            .unwrap();
        assert_eq!(store.user_by_cookie("cookie-1").unwrap().id, user.id);
        store
            .create_oidc_session("cookie-2", user.id, Duration::ZERO)
            .unwrap();
        assert!(store.user_by_cookie("cookie-2").is_err());
        store.delete_oidc_session("cookie-1").unwrap();
        assert!(store.user_by_cookie("cookie-1").is_err());

        // Grants: set, read, upgrade, revoke.
        let instance = store
            .register_instance("g-host", &hash_token("reg"))
            .unwrap();
        assert_eq!(store.grant_for(user.id, instance).unwrap(), None);
        store.set_grant(user.id, instance, Role::Viewer).unwrap();
        assert_eq!(
            store.grant_for(user.id, instance).unwrap(),
            Some(Role::Viewer)
        );
        store
            .set_grant(user.id, instance, Role::Controller)
            .unwrap();
        assert_eq!(
            store.grant_for(user.id, instance).unwrap(),
            Some(Role::Controller)
        );
        store.set_grant(second.id, instance, Role::Viewer).unwrap();
        assert_eq!(store.list_grants().unwrap().len(), 2);
        assert!(store.delete_grant(user.id, instance).unwrap());
        assert_eq!(store.grant_for(user.id, instance).unwrap(), None);
    }

    #[test]
    fn user_by_sub_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        store.upsert_user("sub-x", Some("x@x"), None).unwrap();
        let found = store.user_by_sub("sub-x").unwrap();
        assert_eq!(found.email.as_deref(), Some("x@x"));
        assert!(store.user_by_sub("missing").is_err());
    }

    #[test]
    fn link_expiry_and_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        let id = store
            .register_instance("host-a", &hash_token("reg"))
            .unwrap();
        store
            .create_link("link-1", id, None, "viewer", Duration::ZERO)
            .unwrap();
        assert!(store.link_by_token("link-1").is_err());

        store
            .create_link(
                "link-2",
                id,
                None,
                "viewer",
                Duration::from_secs(2 * 60 * 60),
            )
            .unwrap();
        assert!(store.link_by_token("link-2").is_ok());
        let link = store.link_by_token("link-2").unwrap();
        assert_eq!(link.rights, "viewer");
        assert!(store.revoke_link(&hash_token("link-2")).unwrap());
        assert!(store.link_by_token("link-2").is_err());
    }

    #[test]
    fn extend_link_keeps_revoked_links_dead() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        let id = store
            .register_instance("host-a", &hash_token("reg"))
            .unwrap();
        store
            .create_link("live", id, None, "controller", Duration::from_secs(60))
            .unwrap();
        store
            .create_link("dead", id, None, "controller", Duration::from_secs(60))
            .unwrap();
        store.revoke_link(&hash_token("dead")).unwrap();

        store
            .extend_link(&hash_token("live"), Duration::from_secs(7200))
            .unwrap();
        store
            .extend_link(&hash_token("dead"), Duration::from_secs(7200))
            .unwrap();
        assert!(store.link_by_token("live").is_ok());
        assert!(store.link_by_token("dead").is_err());
    }

    #[test]
    fn live_control_link_survives_traffic_and_dies_on_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        let id = store
            .register_instance("host-a", &hash_token("reg"))
            .unwrap();
        assert!(
            store.live_control_link(id).unwrap().is_none(),
            "no link yet"
        );
        store
            .create_link("ctl-live", id, None, "controller", Duration::from_secs(60))
            .unwrap();
        store
            .create_link("ctl-dead", id, None, "controller", Duration::ZERO)
            .unwrap();
        store
            .create_link(
                "scoped",
                id,
                Some("s1"),
                "controller",
                Duration::from_secs(60),
            )
            .unwrap();
        store
            .create_link("viewer", id, None, "viewer", Duration::from_secs(60))
            .unwrap();
        let live = store.live_control_link(id).unwrap().expect("the live one");
        assert_eq!(live.token, "ctl-live", "scoped and viewer links stay out");
        store.revoke_link(&hash_token("ctl-live")).unwrap();
        assert!(
            store.live_control_link(id).unwrap().is_none(),
            "revocation ends the reuse"
        );
    }

    #[test]
    fn create_instance_refuses_existing_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        let id = store.create_instance("dup", &hash_token("t1")).unwrap();
        assert!(store.create_instance("dup", &hash_token("t2")).is_err());
        // The original token still authenticates; rotation is CLI-only.
        let row = store.instance_by_registration_token("t1").unwrap();
        assert_eq!(row.id, id);
        assert!(store.instance_by_registration_token("t2").is_err());
    }

    #[test]
    fn register_instance_rotates_cli_side() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        let first = store.register_instance("host", &hash_token("t1")).unwrap();
        let second = store.register_instance("host", &hash_token("t2")).unwrap();
        assert_eq!(first, second, "re-registering the same name reuses the row");
        assert!(store.instance_by_registration_token("t1").is_err());
        assert_eq!(
            store.instance_by_registration_token("t2").unwrap().name,
            "host"
        );
    }

    #[test]
    fn deleting_the_last_admin_does_not_promote_whoever_logs_in_next() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        let first = store.upsert_user("sub-1", None, None).unwrap();
        assert!(first.is_admin, "fresh deployment bootstraps");
        store.upsert_user("sub-2", None, None).unwrap();
        // Deleting the only admin must not reopen the bootstrap while other
        // users remain: role restoration is an explicit act, not a race.
        store.delete_user(first.id).unwrap();
        let next = store.upsert_user("sub-2", None, None).unwrap();
        assert!(!next.is_admin);
        let newcomer = store.upsert_user("sub-9", None, None).unwrap();
        assert!(!newcomer.is_admin);
        store.set_user_admin(newcomer.id, true).unwrap();
        assert!(store.user_by_id(newcomer.id).unwrap().is_admin);
        // A fully emptied table is a fresh deployment again.
        for u in store.list_users().unwrap() {
            store.delete_user(u.id).unwrap();
        }
        assert!(store.upsert_user("sub-3", None, None).unwrap().is_admin);
    }

    #[test]
    fn local_users_hash_with_argon_and_verify() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        let first = store
            .create_local_user("ops", "hunter2-hunter2", None, None, false)
            .unwrap();
        assert!(first.is_admin, "first user bootstraps to admin");
        let second = store
            .create_local_user("peer", "other-password", None, None, false)
            .unwrap();
        assert!(!second.is_admin);
        assert!(store.verify_local_user("peer", "other-password").is_ok());
    }

    #[test]
    fn legacy_passwords_upgrade_on_verify() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        store
            .create_local_user("old", "unused-unused", None, None, true)
            .unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE users SET password_hash = ?1 WHERE local_username = ?2",
                rusqlite::params![Store::legacy_password_hash("old", "hunter2-hunter2"), "old"],
            )
            .unwrap();
        }
        assert!(store.verify_local_user("old", "hunter2-hunter2").is_ok());
        let conn = store.conn.lock().unwrap();
        let stored: String = conn
            .query_row(
                "SELECT password_hash FROM users WHERE local_username = 'old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(stored.starts_with("$argon2"), "upgraded in place");
    }

    #[test]
    fn local_user_wrong_password_finds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        store
            .create_local_user("bob", "right-password", None, None, false)
            .unwrap();
        assert!(store.verify_local_user("bob", "wrong-password").is_err());
        assert!(store.verify_local_user("nobody", "right-password").is_err());
    }

    #[test]
    fn instance_names_validate_charset() {
        assert!(valid_instance_name("work-laptop_1.local"));
        assert!(!valid_instance_name(""));
        assert!(!valid_instance_name("x/y"));
        assert!(!valid_instance_name("<script>"));
        assert!(!valid_instance_name(&"a".repeat(65)));
    }
}
