//! SQLite persistence for the hub.
//!
//! One database file holds everything: users, sessions, guilds, telescopes,
//! credentials, and updater state. Access goes through a single
//! `Arc<Mutex<Connection>>`; rusqlite is synchronous and every query here is
//! short, so a plain mutex is enough.
//!
//! Migrations are a hand-rolled list keyed off `PRAGMA user_version` rather
//! than a migration crate, to avoid coupling our rusqlite version to a
//! third-party crate's supported range (rusqlite itself must stay on the same
//! libsqlite3-sys line as matrix-sdk).

use rusqlite::Connection;
use rusqlite::OptionalExtension;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Ordered schema migrations. Entry at index `i` brings the schema to
/// `user_version = i + 1`. Append only — never edit or reorder a shipped
/// entry.
const MIGRATIONS: &[&str] = &[
    // V1: instance-level key/value settings.
    "CREATE TABLE hub_settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    ) STRICT;",
    // V2: Discord login. Users, their guild snapshot from OAuth, server-side
    // sessions, and single-use OAuth state rows. Snowflakes are stored as
    // INTEGER (u64 bit-cast to i64) and serialized to JSON as strings.
    "CREATE TABLE users (
        discord_user_id INTEGER PRIMARY KEY,
        username TEXT NOT NULL,
        email TEXT,
        email_verified INTEGER NOT NULL DEFAULT 0,
        avatar_url TEXT,
        created_at INTEGER NOT NULL,
        last_auth_at INTEGER NOT NULL
    ) STRICT;
    CREATE TABLE user_guilds (
        discord_user_id INTEGER NOT NULL
            REFERENCES users(discord_user_id) ON DELETE CASCADE,
        guild_id INTEGER NOT NULL,
        guild_name TEXT NOT NULL,
        permissions INTEGER NOT NULL,
        is_owner INTEGER NOT NULL DEFAULT 0,
        last_seen_at INTEGER NOT NULL,
        PRIMARY KEY (discord_user_id, guild_id)
    ) STRICT;
    CREATE INDEX idx_user_guilds_guild ON user_guilds(guild_id);
    CREATE TABLE sessions (
        session_id TEXT PRIMARY KEY,
        csrf_token TEXT NOT NULL,
        discord_user_id INTEGER NOT NULL
            REFERENCES users(discord_user_id) ON DELETE CASCADE,
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX idx_sessions_user ON sessions(discord_user_id);
    CREATE TABLE oauth_states (
        nonce TEXT PRIMARY KEY,
        pkce_verifier TEXT NOT NULL,
        next_path TEXT NOT NULL,
        issued_at INTEGER NOT NULL,
        consumed_at INTEGER
    ) STRICT;",
    // V3: tenancy. A Discord guild is a tenant; telescopes belong to a guild;
    // pairing tokens are one-time secrets exchanged for rig credentials.
    // Only the SHA-256 of a pairing token is stored.
    "CREATE TABLE guilds (
        guild_id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        registered_by INTEGER NOT NULL REFERENCES users(discord_user_id),
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    ) STRICT;
    CREATE TABLE telescopes (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        guild_id INTEGER NOT NULL REFERENCES guilds(guild_id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        discord_channel_id INTEGER,
        image_cooldown_seconds INTEGER NOT NULL DEFAULT 60,
        write_policy TEXT NOT NULL DEFAULT 'disabled'
            CHECK (write_policy IN ('disabled', 'roles')),
        allowed_role_ids TEXT NOT NULL DEFAULT '[]',
        created_by INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        UNIQUE (guild_id, name)
    ) STRICT;
    CREATE INDEX idx_telescopes_guild ON telescopes(guild_id);
    CREATE TABLE pairing_tokens (
        token_hash TEXT PRIMARY KEY,
        telescope_id INTEGER NOT NULL REFERENCES telescopes(id) ON DELETE CASCADE,
        created_by INTEGER NOT NULL,
        issued_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        consumed_at INTEGER
    ) STRICT;
    CREATE INDEX idx_pairing_tokens_telescope ON pairing_tokens(telescope_id);",
    // V4: durable rig credentials minted by the pairing exchange. Only the
    // SHA-256 of a credential is stored; the node/profile binding pins a
    // credential to the installation that paired it.
    "CREATE TABLE rig_credentials (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        telescope_id INTEGER NOT NULL REFERENCES telescopes(id) ON DELETE CASCADE,
        credential_hash TEXT NOT NULL UNIQUE,
        node_id TEXT NOT NULL,
        profile_id TEXT NOT NULL,
        paired_at INTEGER NOT NULL,
        last_seen_at INTEGER NOT NULL,
        revoked_at INTEGER
    ) STRICT;
    CREATE INDEX idx_rig_credentials_telescope ON rig_credentials(telescope_id);",
    // V5: audit trail of management actions, queryable per guild.
    "CREATE TABLE audit_log (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        at INTEGER NOT NULL,
        discord_user_id INTEGER NOT NULL,
        guild_id INTEGER NOT NULL,
        action TEXT NOT NULL,
        detail TEXT NOT NULL DEFAULT ''
    ) STRICT;
    CREATE INDEX idx_audit_guild ON audit_log(guild_id, at);",
    // V6: a Discord channel routes to at most one telescope, across all
    // guilds. Channel routing is the command/notification identity, so two
    // claims on one channel would cross tenants.
    "CREATE UNIQUE INDEX idx_telescopes_channel_unique
        ON telescopes(discord_channel_id) WHERE discord_channel_id IS NOT NULL;",
    // V7: write_policy gains 'admins' (guild owner/managers may run write
    // commands) and it becomes the default, so the integration's owner
    // controls their scopes from Discord without any allowlist setup.
    // SQLite cannot alter a CHECK, so the table is rebuilt; migrations run
    // with foreign keys off and an integrity check afterwards.
    "CREATE TABLE telescopes_v7 (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        guild_id INTEGER NOT NULL REFERENCES guilds(guild_id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        discord_channel_id INTEGER,
        image_cooldown_seconds INTEGER NOT NULL DEFAULT 60,
        write_policy TEXT NOT NULL DEFAULT 'admins'
            CHECK (write_policy IN ('disabled', 'admins', 'roles')),
        allowed_role_ids TEXT NOT NULL DEFAULT '[]',
        created_by INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        UNIQUE (guild_id, name)
    ) STRICT;
    INSERT INTO telescopes_v7
        SELECT id, guild_id, name, discord_channel_id, image_cooldown_seconds,
               write_policy, allowed_role_ids, created_by, created_at
        FROM telescopes;
    DROP TABLE telescopes;
    ALTER TABLE telescopes_v7 RENAME TO telescopes;
    CREATE INDEX idx_telescopes_guild ON telescopes(guild_id);
    CREATE UNIQUE INDEX idx_telescopes_channel_unique
        ON telescopes(discord_channel_id) WHERE discord_channel_id IS NOT NULL;",
    // V8: one telescope, many destinations — including channels in other
    // guilds (added by that guild's own manager via a share code). Routing
    // moves from a column to the telescope_channels table; a channel still
    // maps to exactly one telescope so slash commands stay unambiguous.
    // telescope_shares holds single-use codes for cross-server subscribing.
    "CREATE TABLE telescope_channels (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        telescope_id INTEGER NOT NULL REFERENCES telescopes(id) ON DELETE CASCADE,
        guild_id INTEGER NOT NULL REFERENCES guilds(guild_id) ON DELETE CASCADE,
        channel_id INTEGER NOT NULL UNIQUE,
        channel_name TEXT NOT NULL DEFAULT '',
        guild_name TEXT NOT NULL DEFAULT '',
        created_by INTEGER NOT NULL,
        created_at INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX idx_telescope_channels_telescope
        ON telescope_channels(telescope_id);
    CREATE INDEX idx_telescope_channels_guild ON telescope_channels(guild_id);
    INSERT INTO telescope_channels
        (telescope_id, guild_id, channel_id, created_by, created_at)
        SELECT id, guild_id, discord_channel_id, created_by, created_at
        FROM telescopes WHERE discord_channel_id IS NOT NULL;
    CREATE TABLE telescopes_v8 (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        guild_id INTEGER NOT NULL REFERENCES guilds(guild_id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        image_cooldown_seconds INTEGER NOT NULL DEFAULT 60,
        write_policy TEXT NOT NULL DEFAULT 'admins'
            CHECK (write_policy IN ('disabled', 'admins', 'roles')),
        allowed_role_ids TEXT NOT NULL DEFAULT '[]',
        created_by INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        UNIQUE (guild_id, name)
    ) STRICT;
    INSERT INTO telescopes_v8
        SELECT id, guild_id, name, image_cooldown_seconds, write_policy,
               allowed_role_ids, created_by, created_at
        FROM telescopes;
    DROP TABLE telescopes;
    ALTER TABLE telescopes_v8 RENAME TO telescopes;
    CREATE INDEX idx_telescopes_guild ON telescopes(guild_id);
    CREATE TABLE telescope_shares (
        code_hash TEXT PRIMARY KEY,
        telescope_id INTEGER NOT NULL REFERENCES telescopes(id) ON DELETE CASCADE,
        created_by INTEGER NOT NULL,
        issued_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        consumed_at INTEGER
    ) STRICT;",
    // V9: telescopes belong to a USER, not a guild. Each server relationship
    // is an explicit attachment carrying that server's capability and
    // permissions: can_command distinguishes a full attachment (made by the
    // owner in a server they manage) from a feed-only subscription (made
    // with a share code). Channel routes belong to the attachment's guild.
    // Existing telescopes: created_by becomes the owner (names deduped per
    // owner), the owning guild becomes a can_command attachment with the old
    // policy, and cross-guild V8 routes become feed-only attachments.
    "CREATE TABLE telescopes_v9 (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        owner_id INTEGER NOT NULL REFERENCES users(discord_user_id),
        name TEXT NOT NULL,
        image_cooldown_seconds INTEGER NOT NULL DEFAULT 60,
        created_at INTEGER NOT NULL,
        UNIQUE (owner_id, name)
    ) STRICT;
    INSERT INTO telescopes_v9 (id, owner_id, name, image_cooldown_seconds, created_at)
        SELECT t.id, t.created_by,
               CASE WHEN EXISTS (
                   SELECT 1 FROM telescopes t2
                   WHERE t2.created_by = t.created_by AND t2.name = t.name
                         AND t2.id < t.id)
                    THEN t.name || '-' || t.id ELSE t.name END,
               t.image_cooldown_seconds, t.created_at
        FROM telescopes t;
    CREATE TABLE telescope_attachments (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        telescope_id INTEGER NOT NULL REFERENCES telescopes(id) ON DELETE CASCADE,
        guild_id INTEGER NOT NULL REFERENCES guilds(guild_id) ON DELETE CASCADE,
        can_command INTEGER NOT NULL DEFAULT 0,
        write_policy TEXT NOT NULL DEFAULT 'admins'
            CHECK (write_policy IN ('disabled', 'admins', 'roles')),
        allowed_role_ids TEXT NOT NULL DEFAULT '[]',
        created_by INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        UNIQUE (telescope_id, guild_id)
    ) STRICT;
    CREATE INDEX idx_attachments_guild ON telescope_attachments(guild_id);
    INSERT INTO telescope_attachments
        (telescope_id, guild_id, can_command, write_policy, allowed_role_ids,
         created_by, created_at)
        SELECT id, guild_id, 1, write_policy, allowed_role_ids, created_by, created_at
        FROM telescopes;
    INSERT INTO telescope_attachments
        (telescope_id, guild_id, can_command, write_policy, allowed_role_ids,
         created_by, created_at)
        SELECT DISTINCT tc.telescope_id, tc.guild_id, 0, 'disabled', '[]',
               tc.created_by, tc.created_at
        FROM telescope_channels tc
        JOIN telescopes t ON t.id = tc.telescope_id
        WHERE tc.guild_id != t.guild_id;
    DROP TABLE telescopes;
    ALTER TABLE telescopes_v9 RENAME TO telescopes;
    CREATE INDEX idx_telescopes_owner ON telescopes(owner_id);",
    // V10: successful autofocus deliveries survive Hub restarts. Keep only
    // the normalized report identity, never report contents or equipment
    // metadata. Do not expire old identities: plugins may retain the latest
    // report indefinitely. Telescope deletion removes its delivery history.
    "CREATE TABLE autofocus_deliveries (
        telescope_id INTEGER NOT NULL REFERENCES telescopes(id) ON DELETE CASCADE,
        profile_id TEXT NOT NULL,
        channel_id INTEGER NOT NULL,
        report_identity TEXT NOT NULL,
        delivered_at INTEGER NOT NULL,
        PRIMARY KEY (telescope_id, profile_id, channel_id, report_identity)
    ) STRICT;",
];

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database mutex poisoned")]
    Poisoned,
    #[error("database integrity error: {0}")]
    Integrity(String),
}

/// Current unix time in seconds, for `*_at` columns. One clock definition
/// for the whole crate (the Direct expiry contract uses the same one).
pub use crate::direct::protocol::unix_now;

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, DbError> {
        Self::from_connection(Connection::open(path)?)
    }

    /// In-memory database for tests.
    pub fn open_in_memory() -> Result<Self, DbError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self, DbError> {
        // `PRAGMA journal_mode` returns a row (the resulting mode), so it
        // goes through query_row instead of pragma_update.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Migrations run with foreign keys off so table rebuilds (drop +
        // rename) don't trip enforcement mid-surgery; the integrity check
        // below catches anything a migration actually broke.
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        migrate(&conn)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let violation: Option<String> = conn
            .query_row("PRAGMA foreign_key_check", [], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        if let Some(table) = violation {
            return Err(DbError::Integrity(format!(
                "foreign key violation in table '{table}' after migration"
            )));
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run a closure against the connection under the lock.
    pub fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Result<T, DbError> {
        let guard = self.conn.lock().map_err(|_| DbError::Poisoned)?;
        f(&guard).map_err(DbError::from)
    }

    pub fn schema_version(&self) -> Result<u32, DbError> {
        self.with_conn(|conn| conn.query_row("PRAGMA user_version", [], |r| r.get(0)))
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO hub_settings (key, value, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3",
                rusqlite::params![key, value, unix_now()],
            )
            .map(|_| ())
        })
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>, DbError> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT value FROM hub_settings WHERE key = ?1",
                rusqlite::params![key],
                |r| r.get(0),
            )
            .optional()
        })
    }
}

/// One audit trail entry.
#[derive(Debug, Clone)]
pub struct AuditRow {
    pub at: i64,
    pub discord_user_id: i64,
    pub action: String,
    pub detail: String,
}

impl Db {
    /// Record a management action. Failures are logged, never propagated —
    /// auditing must not break the action it records.
    pub fn audit(&self, discord_user_id: i64, guild_id: i64, action: &str, detail: &str) {
        let result = self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO audit_log (at, discord_user_id, guild_id, action, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![unix_now(), discord_user_id, guild_id, action, detail],
            )
            .map(|_| ())
        });
        if let Err(e) = result {
            eprintln!("Warning: audit write failed: {e}");
        }
    }

    /// Newest-first audit entries for one guild.
    pub fn guild_audit(&self, guild_id: i64, limit: u32) -> Result<Vec<AuditRow>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT at, discord_user_id, action, detail FROM audit_log
                 WHERE guild_id = ?1 ORDER BY at DESC, id DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![guild_id, limit], |r| {
                Ok(AuditRow {
                    at: r.get(0)?,
                    discord_user_id: r.get(1)?,
                    action: r.get(2)?,
                    detail: r.get(3)?,
                })
            })?;
            rows.collect()
        })
    }
}

/// Apply any migrations beyond the database's current `user_version`. Each
/// migration and its version bump commit atomically, so a crash mid-migration
/// leaves the previous version intact.
fn migrate(conn: &Connection) -> Result<(), DbError> {
    let current: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = i + 1;
        conn.execute_batch(&format!(
            "BEGIN;\n{sql}\nPRAGMA user_version = {version};\nCOMMIT;"
        ))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_db_migrates_to_latest() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap() as usize, MIGRATIONS.len());
    }

    #[test]
    fn migrate_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        db.with_conn(|conn| {
            migrate(conn).expect("re-running migrations must be a no-op");
            Ok(())
        })
        .unwrap();
        assert_eq!(db.schema_version().unwrap() as usize, MIGRATIONS.len());
    }

    #[test]
    fn migrate_persists_across_reopen() {
        let dir = std::env::temp_dir().join(format!("chatstronomy-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.db");
        let _ = std::fs::remove_file(&path);

        {
            let db = Db::open(&path).unwrap();
            db.set_setting("instance_name", "test-hub").unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap() as usize, MIGRATIONS.len());
        assert_eq!(
            db.get_setting("instance_name").unwrap().as_deref(),
            Some("test-hub")
        );

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upgrade_from_v6_preserves_rows_and_migrates_routing() {
        // Build a V6-era database by hand, populate it, then run the full
        // migration chain over it. Foreign keys off, exactly as
        // from_connection runs migrations — otherwise the rebuilds' DROP
        // TABLE would cascade-delete the credential rows.
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        for (i, sql) in MIGRATIONS.iter().take(6).enumerate() {
            conn.execute_batch(&format!(
                "BEGIN;\n{sql}\nPRAGMA user_version = {};\nCOMMIT;",
                i + 1
            ))
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO users (discord_user_id, username, created_at, last_auth_at)
                 VALUES (1, 'admin', 0, 0);
             INSERT INTO guilds (guild_id, name, registered_by, created_at, updated_at)
                 VALUES (100, 'g', 1, 0, 0);
             INSERT INTO telescopes
                 (guild_id, name, discord_channel_id, write_policy, allowed_role_ids,
                  created_by, created_at)
                 VALUES (100, 'c925', 42, 'roles', '[7]', 1, 0);
             INSERT INTO rig_credentials
                 (telescope_id, credential_hash, node_id, profile_id, paired_at, last_seen_at)
                 VALUES (1, 'hash', 'n', 'p', 0, 0);",
        )
        .unwrap();

        migrate(&conn).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, usize>(0))
                .unwrap(),
            MIGRATIONS.len()
        );

        // V9: created_by became the owner; the old per-telescope policy
        // moved onto a can_command attachment for the old owning guild.
        let owner: i64 = conn
            .query_row(
                "SELECT owner_id FROM telescopes WHERE name = 'c925'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, 1);
        let (can_command, policy, roles): (bool, String, String) = conn
            .query_row(
                "SELECT can_command, write_policy, allowed_role_ids
                 FROM telescope_attachments WHERE guild_id = 100",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(can_command);
        assert_eq!((policy.as_str(), roles.as_str()), ("roles", "[7]"));

        // V8 moved the routed channel into telescope_channels.
        let (route_guild, route_channel): (i64, i64) = conn
            .query_row(
                "SELECT tc.guild_id, tc.channel_id FROM telescope_channels tc
                 JOIN telescopes t ON t.id = tc.telescope_id
                 WHERE t.name = 'c925'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((route_guild, route_channel), (100, 42));

        // The credential still points at the rebuilt row.
        let credentials: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM rig_credentials rc
                 JOIN telescopes t ON t.id = rc.telescope_id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(credentials, 1);

        // New attachments default to the admins policy.
        conn.execute(
            "INSERT INTO telescope_attachments
                 (telescope_id, guild_id, can_command, created_by, created_at)
             VALUES (1, 100, 1, 1, 0)",
            [],
        )
        .ok(); // unique (telescope, guild) may already exist from migration
        let default_policy: String = conn
            .query_row(
                "SELECT write_policy FROM telescope_attachments LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(["admins", "roles"].contains(&default_policy.as_str()));

        // A channel routes to exactly one telescope, still enforced (V8).
        let clash = conn.execute(
            "INSERT INTO telescope_channels
                 (telescope_id, guild_id, channel_id, created_by, created_at)
             VALUES (2, 100, 42, 1, 0)",
            [],
        );
        assert!(clash.is_err());
    }

    #[test]
    fn settings_upsert_and_missing() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.get_setting("missing").unwrap(), None);
        db.set_setting("k", "v1").unwrap();
        db.set_setting("k", "v2").unwrap();
        assert_eq!(db.get_setting("k").unwrap().as_deref(), Some("v2"));
    }

    #[test]
    fn upgrade_from_v9_preserves_telescope_and_adds_autofocus_ledger() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        for (i, sql) in MIGRATIONS.iter().take(9).enumerate() {
            conn.execute_batch(&format!(
                "BEGIN;\n{sql}\nPRAGMA user_version = {};\nCOMMIT;",
                i + 1
            ))
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO users (discord_user_id, username, created_at, last_auth_at)
                 VALUES (1, 'owner', 0, 0);
             INSERT INTO telescopes (id, owner_id, name, created_at)
                 VALUES (1, 1, 'retained telescope', 0);",
        )
        .unwrap();

        let db = Db::from_connection(conn).unwrap();
        assert_eq!(db.schema_version().unwrap() as usize, MIGRATIONS.len());
        let name: String = db
            .with_conn(|conn| {
                conn.query_row("SELECT name FROM telescopes WHERE id = 1", [], |r| r.get(0))
            })
            .unwrap();
        assert_eq!(name, "retained telescope");
        assert!(!db.autofocus_delivered(1, "profile", 100, "report").unwrap());
        db.with_conn(|conn| {
            migrate(conn).expect("migration remains idempotent after the ledger upgrade");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn foreign_keys_enabled() {
        let db = Db::open_in_memory().unwrap();
        let on: i64 = db
            .with_conn(|conn| conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(on, 1);
    }
}
