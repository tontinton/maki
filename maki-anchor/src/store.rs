use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::Connection;
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("not found")]
    NotFound,
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

/// Both arguments must be presented in the same form; hashing one raw token
/// against an already-hashed value would never match.
pub fn tokens_equal_constant_time(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(serde::Serialize)]
pub struct InstanceRow {
    pub id: i64,
    pub name: String,
    pub last_seen: i64,
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

pub struct LinkRow {
    pub token_hash: String,
    pub instance_id: i64,
    pub external_session_id: Option<String>,
    pub rights: String,
    pub expires_at: i64,
}

impl Store {
    pub fn open(path: &Path) -> Result<Arc<Self>, StoreError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
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
                 instance_id INTEGER NOT NULL REFERENCES instances(id) ON DELETE CASCADE,
                 external_session_id TEXT,
                 rights TEXT NOT NULL,
                 expires_at INTEGER NOT NULL,
                 revoked_at INTEGER
             );",
        )?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
        }))
    }

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
        Ok(conn.last_insert_rowid())
    }

    pub fn instance_by_registration_token(&self, token: &str) -> Result<InstanceRow, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, name, registration_token_hash, last_seen FROM instances")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        let presented = hash_token(token);
        for (id, name, stored_hash, last_seen) in rows {
            if tokens_equal_constant_time(&presented, &stored_hash) {
                return Ok(InstanceRow {
                    id,
                    name,
                    last_seen,
                });
            }
        }
        Err(StoreError::NotFound)
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

    pub fn create_link(
        &self,
        token_hash: &str,
        instance_id: i64,
        external_session_id: Option<&str>,
        rights: &str,
        ttl: Duration,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO links (token_hash, instance_id, external_session_id, rights, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                token_hash,
                instance_id,
                external_session_id,
                rights,
                now_unix() + ttl.as_secs() as i64,
            ],
        )?;
        Ok(())
    }

    pub fn link_by_token(&self, token: &str) -> Result<LinkRow, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT token_hash, instance_id, external_session_id, rights, expires_at
             FROM links WHERE revoked_at IS NULL",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(LinkRow {
                    token_hash: row.get(0)?,
                    instance_id: row.get(1)?,
                    external_session_id: row.get(2)?,
                    rights: row.get(3)?,
                    expires_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        let presented = hash_token(token);
        for link in rows {
            let matched = tokens_equal_constant_time(&presented, &link.token_hash);
            if matched {
                if link.expires_at <= now_unix() {
                    return Err(StoreError::NotFound);
                }
                return Ok(link);
            }
        }
        Err(StoreError::NotFound)
    }

    pub fn revoke_link(&self, token_hash: &str) -> Result<bool, StoreError> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE links SET revoked_at = ?1 WHERE token_hash = ?2 AND revoked_at IS NULL",
            rusqlite::params![now_unix(), token_hash],
        )?;
        Ok(changed > 0)
    }
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
    fn link_expiry_and_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        let id = store
            .register_instance("host-a", &hash_token("reg"))
            .unwrap();
        store
            .create_link(&hash_token("link-1"), id, None, "viewer", Duration::ZERO)
            .unwrap();
        assert!(store.link_by_token("link-1").is_err());

        store
            .create_link(
                &hash_token("link-2"),
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
}
