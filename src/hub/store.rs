//! Login-related persistence: users, guild snapshots, sessions, and
//! single-use OAuth state rows.

use super::auth::OAUTH_STATE_MAX_AGE_SECONDS;
use super::db::{Db, DbError, unix_now};
use rusqlite::OptionalExtension;
use uuid::Uuid;

/// A user's guild membership as captured at OAuth time.
#[derive(Debug, Clone)]
pub struct GuildSnapshot {
    pub guild_id: i64,
    pub guild_name: String,
    pub permissions: i64,
    pub is_owner: bool,
}

#[derive(Clone)]
pub struct SessionRow {
    pub session_id: String,
    pub csrf_token: String,
    pub discord_user_id: i64,
    pub expires_at: i64,
}

impl std::fmt::Debug for SessionRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRow")
            .field(
                "session_id",
                &crate::security::secret_marker(&self.session_id),
            )
            .field(
                "csrf_token",
                &crate::security::secret_marker(&self.csrf_token),
            )
            .field("discord_user_id", &self.discord_user_id)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct UserRow {
    pub discord_user_id: i64,
    pub username: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub avatar_url: Option<String>,
}

impl Db {
    /// Mint a single-use OAuth state row and return its nonce.
    pub fn begin_oauth_state(
        &self,
        pkce_verifier: &str,
        next_path: &str,
    ) -> Result<String, DbError> {
        let nonce = Uuid::new_v4().to_string();
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO oauth_states (nonce, pkce_verifier, next_path, issued_at)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![nonce, pkce_verifier, next_path, unix_now()],
            )
            .map(|_| ())
        })?;
        Ok(nonce)
    }

    /// Consume an OAuth state row. Returns the PKCE verifier and next path
    /// only the first time, and only within the state's lifetime.
    pub fn consume_oauth_state(&self, nonce: &str) -> Result<Option<(String, String)>, DbError> {
        let now = unix_now();
        self.with_conn(|conn| {
            let row: Option<(String, String, i64, Option<i64>)> = conn
                .query_row(
                    "SELECT pkce_verifier, next_path, issued_at, consumed_at
                     FROM oauth_states WHERE nonce = ?1",
                    rusqlite::params![nonce],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let Some((verifier, next_path, issued_at, consumed_at)) = row else {
                return Ok(None);
            };
            if consumed_at.is_some() || now - issued_at > OAUTH_STATE_MAX_AGE_SECONDS {
                return Ok(None);
            }
            conn.execute(
                "UPDATE oauth_states SET consumed_at = ?1 WHERE nonce = ?2",
                rusqlite::params![now, nonce],
            )?;
            Ok(Some((verifier, next_path)))
        })
    }

    /// Insert or refresh a user after a successful login.
    pub fn upsert_user(&self, user: &UserRow) -> Result<(), DbError> {
        let now = unix_now();
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO users
                     (discord_user_id, username, email, email_verified, avatar_url,
                      created_at, last_auth_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                 ON CONFLICT(discord_user_id) DO UPDATE SET
                     username = ?2, email = ?3, email_verified = ?4,
                     avatar_url = ?5, last_auth_at = ?6",
                rusqlite::params![
                    user.discord_user_id,
                    user.username,
                    user.email,
                    user.email_verified,
                    user.avatar_url,
                    now
                ],
            )
            .map(|_| ())
        })
    }

    pub fn get_user(&self, discord_user_id: i64) -> Result<Option<UserRow>, DbError> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT discord_user_id, username, email, email_verified, avatar_url
                 FROM users WHERE discord_user_id = ?1",
                rusqlite::params![discord_user_id],
                |r| {
                    Ok(UserRow {
                        discord_user_id: r.get(0)?,
                        username: r.get(1)?,
                        email: r.get(2)?,
                        email_verified: r.get(3)?,
                        avatar_url: r.get(4)?,
                    })
                },
            )
            .optional()
        })
    }

    /// Replace a user's guild snapshot in one transaction.
    pub fn replace_user_guilds(
        &self,
        discord_user_id: i64,
        guilds: &[GuildSnapshot],
    ) -> Result<(), DbError> {
        let now = unix_now();
        self.with_conn(|conn| {
            conn.execute_batch("BEGIN")?;
            let result = (|| {
                conn.execute(
                    "DELETE FROM user_guilds WHERE discord_user_id = ?1",
                    rusqlite::params![discord_user_id],
                )?;
                for g in guilds {
                    conn.execute(
                        "INSERT INTO user_guilds
                             (discord_user_id, guild_id, guild_name, permissions,
                              is_owner, last_seen_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        rusqlite::params![
                            discord_user_id,
                            g.guild_id,
                            g.guild_name,
                            g.permissions,
                            g.is_owner,
                            now
                        ],
                    )?;
                }
                Ok(())
            })();
            match result {
                Ok(()) => conn.execute_batch("COMMIT")?,
                Err(_) => conn.execute_batch("ROLLBACK")?,
            }
            result
        })
    }

    pub fn user_guilds(&self, discord_user_id: i64) -> Result<Vec<GuildSnapshot>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT guild_id, guild_name, permissions, is_owner
                 FROM user_guilds WHERE discord_user_id = ?1 ORDER BY guild_name",
            )?;
            let rows = stmt.query_map(rusqlite::params![discord_user_id], |r| {
                Ok(GuildSnapshot {
                    guild_id: r.get(0)?,
                    guild_name: r.get(1)?,
                    permissions: r.get(2)?,
                    is_owner: r.get(3)?,
                })
            })?;
            rows.collect()
        })
    }

    /// Create a fresh session for a user and return it.
    pub fn create_session(
        &self,
        discord_user_id: i64,
        session_hours: u64,
    ) -> Result<SessionRow, DbError> {
        let session = SessionRow {
            session_id: Uuid::new_v4().to_string(),
            csrf_token: Uuid::new_v4().to_string(),
            discord_user_id,
            expires_at: unix_now() + (session_hours as i64) * 3600,
        };
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO sessions
                     (session_id, csrf_token, discord_user_id, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    session.session_id,
                    session.csrf_token,
                    session.discord_user_id,
                    unix_now(),
                    session.expires_at
                ],
            )
            .map(|_| ())
        })?;
        Ok(session)
    }

    /// Look up a session; expired rows are deleted and treated as absent.
    pub fn get_session(&self, session_id: &str) -> Result<Option<SessionRow>, DbError> {
        let now = unix_now();
        self.with_conn(|conn| {
            let row: Option<SessionRow> = conn
                .query_row(
                    "SELECT session_id, csrf_token, discord_user_id, expires_at
                     FROM sessions WHERE session_id = ?1",
                    rusqlite::params![session_id],
                    |r| {
                        Ok(SessionRow {
                            session_id: r.get(0)?,
                            csrf_token: r.get(1)?,
                            discord_user_id: r.get(2)?,
                            expires_at: r.get(3)?,
                        })
                    },
                )
                .optional()?;
            match row {
                Some(s) if s.expires_at <= now => {
                    conn.execute(
                        "DELETE FROM sessions WHERE session_id = ?1",
                        rusqlite::params![session_id],
                    )?;
                    Ok(None)
                }
                other => Ok(other),
            }
        })
    }

    pub fn delete_session(&self, session_id: &str) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM sessions WHERE session_id = ?1",
                rusqlite::params![session_id],
            )
            .map(|_| ())
        })
    }

    /// Drop expired sessions and stale OAuth states. Called opportunistically
    /// from the login flow.
    pub fn cleanup_auth_rows(&self) -> Result<(), DbError> {
        let now = unix_now();
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM sessions WHERE expires_at <= ?1",
                rusqlite::params![now],
            )?;
            conn.execute(
                "DELETE FROM oauth_states
                 WHERE consumed_at IS NOT NULL OR issued_at <= ?1",
                rusqlite::params![now - OAUTH_STATE_MAX_AGE_SECONDS],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_debug_never_exposes_session_or_csrf_secrets() {
        let row = SessionRow {
            session_id: "private-session-cookie".to_string(),
            csrf_token: "private-csrf-token".to_string(),
            discord_user_id: 42,
            expires_at: 123,
        };
        let debug = format!("{row:?}");
        assert!(!debug.contains("private-session-cookie"));
        assert!(!debug.contains("private-csrf-token"));
        assert!(debug.contains("discord_user_id: 42"));
    }

    fn test_user(id: i64) -> UserRow {
        UserRow {
            discord_user_id: id,
            username: format!("user{id}"),
            email: Some(format!("user{id}@example.com")),
            email_verified: true,
            avatar_url: None,
        }
    }

    #[test]
    fn oauth_state_single_use() {
        let db = Db::open_in_memory().unwrap();
        let nonce = db.begin_oauth_state("verifier-1", "/next").unwrap();
        let (verifier, next) = db.consume_oauth_state(&nonce).unwrap().unwrap();
        assert_eq!(verifier, "verifier-1");
        assert_eq!(next, "/next");
        // Second consumption fails.
        assert!(db.consume_oauth_state(&nonce).unwrap().is_none());
        // Unknown nonce fails.
        assert!(db.consume_oauth_state("nope").unwrap().is_none());
    }

    #[test]
    fn user_upsert_updates_email() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_user(&test_user(1)).unwrap();
        let mut updated = test_user(1);
        updated.email = Some("new@example.com".to_string());
        db.upsert_user(&updated).unwrap();
        let row = db.get_user(1).unwrap().unwrap();
        assert_eq!(row.email.as_deref(), Some("new@example.com"));
        assert!(db.get_user(999).unwrap().is_none());
    }

    #[test]
    fn guild_snapshot_replaced_wholesale() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_user(&test_user(1)).unwrap();
        let g = |id: i64, name: &str| GuildSnapshot {
            guild_id: id,
            guild_name: name.to_string(),
            permissions: 32,
            is_owner: false,
        };
        db.replace_user_guilds(1, &[g(10, "alpha"), g(20, "beta")])
            .unwrap();
        db.replace_user_guilds(1, &[g(20, "beta-renamed")]).unwrap();
        let guilds = db.user_guilds(1).unwrap();
        assert_eq!(guilds.len(), 1);
        assert_eq!(guilds[0].guild_id, 20);
        assert_eq!(guilds[0].guild_name, "beta-renamed");
    }

    #[test]
    fn session_lifecycle() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_user(&test_user(1)).unwrap();
        let session = db.create_session(1, 1).unwrap();
        let loaded = db.get_session(&session.session_id).unwrap().unwrap();
        assert_eq!(loaded.discord_user_id, 1);
        assert_eq!(loaded.csrf_token, session.csrf_token);
        db.delete_session(&session.session_id).unwrap();
        assert!(db.get_session(&session.session_id).unwrap().is_none());
    }

    #[test]
    fn expired_session_deleted_on_read() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_user(&test_user(1)).unwrap();
        let session = db.create_session(1, 1).unwrap();
        // Force expiry in the past.
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE sessions SET expires_at = 1 WHERE session_id = ?1",
                rusqlite::params![session.session_id],
            )
            .map(|_| ())
        })
        .unwrap();
        assert!(db.get_session(&session.session_id).unwrap().is_none());
    }

    #[test]
    fn cleanup_drops_stale_rows() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_user(&test_user(1)).unwrap();
        let session = db.create_session(1, 1).unwrap();
        let nonce = db.begin_oauth_state("v", "/").unwrap();
        db.consume_oauth_state(&nonce).unwrap();
        db.with_conn(|conn| {
            conn.execute("UPDATE sessions SET expires_at = 1", [])
                .map(|_| ())
        })
        .unwrap();
        db.cleanup_auth_rows().unwrap();
        assert!(db.get_session(&session.session_id).unwrap().is_none());
        let states: i64 = db
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM oauth_states", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(states, 0);
    }

    #[test]
    fn deleting_user_cascades() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_user(&test_user(1)).unwrap();
        db.create_session(1, 1).unwrap();
        db.replace_user_guilds(
            1,
            &[GuildSnapshot {
                guild_id: 10,
                guild_name: "g".to_string(),
                permissions: 0,
                is_owner: false,
            }],
        )
        .unwrap();
        db.with_conn(|conn| {
            conn.execute("DELETE FROM users WHERE discord_user_id = 1", [])
                .map(|_| ())
        })
        .unwrap();
        let sessions: i64 = db
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0)))
            .unwrap();
        let guilds: i64 = db
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM user_guilds", [], |r| r.get(0)))
            .unwrap();
        assert_eq!((sessions, guilds), (0, 0));
    }
}
