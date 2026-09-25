//! User-owned, feed-only devices. Deliberately separate from telescope commands.

use super::db::{Db, DbError, unix_now};
use super::tenants::hash_token;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PAIRING_TTL: i64 = 3600;
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    PierCamera,
}

#[derive(Debug, Clone, Serialize)]
pub struct Device {
    pub id: i64,
    #[serde(skip)]
    pub owner_id: i64,
    pub name: String,
    pub kind: DeviceKind,
    pub paired: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceChannel {
    pub id: i64,
    pub device_id: i64,
    // Snowflakes must survive JavaScript's number precision.
    pub guild_id: String,
    pub channel_id: String,
    pub channel_name: String,
}

fn device_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Device> {
    Ok(Device {
        id: row.get(0)?,
        owner_id: row.get(1)?,
        name: row.get(2)?,
        kind: DeviceKind::PierCamera,
        paired: row.get(3)?,
    })
}

const DEVICE_COLUMNS: &str =
    "d.id, d.owner_id, d.name, EXISTS(SELECT 1 FROM device_credentials c WHERE c.device_id=d.id)";

impl Db {
    pub fn create_device(
        &self,
        owner: i64,
        name: &str,
        kind: DeviceKind,
    ) -> Result<Device, DbError> {
        let DeviceKind::PierCamera = kind;
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO devices(owner_id,name,kind,created_at) VALUES(?1,?2,'pier_camera',?3)",
                params![owner, name, unix_now()],
            )?;
            Ok(Device {
                id: c.last_insert_rowid(),
                owner_id: owner,
                name: name.into(),
                kind,
                paired: false,
            })
        })
    }

    pub fn get_device(&self, id: i64) -> Result<Option<Device>, DbError> {
        self.with_conn(|c| {
            c.query_row(
                &format!("SELECT {DEVICE_COLUMNS} FROM devices d WHERE d.id=?1"),
                [id],
                device_row,
            )
            .optional()
        })
    }

    pub fn user_devices(&self, owner: i64) -> Result<Vec<Device>, DbError> {
        self.with_conn(|c| {
            c.prepare(&format!(
                "SELECT {DEVICE_COLUMNS} FROM devices d WHERE d.owner_id=?1 ORDER BY d.id"
            ))?
            .query_map([owner], device_row)?
            .collect()
        })
    }

    pub fn delete_device(&self, id: i64) -> Result<(), DbError> {
        self.with_conn(|c| {
            c.execute("DELETE FROM devices WHERE id=?1", [id])
                .map(|_| ())
        })
    }

    /// Replaces the previous code, but not an already paired credential.
    pub fn issue_device_token(&self, id: i64) -> Result<String, DbError> {
        let token = format!("csdp_{}", super::auth::random_secret());
        self.with_conn(|c| c.execute("INSERT INTO device_pairing_tokens(device_id,token_hash,expires_at) VALUES(?1,?2,?3) ON CONFLICT(device_id) DO UPDATE SET token_hash=excluded.token_hash,expires_at=excluded.expires_at", params![id,hash_token(&token),unix_now()+PAIRING_TTL]).map(|_| ()))?;
        Ok(token)
    }

    /// Consumption and identity-bound credential rotation commit together.
    /// A lost HTTP response requires a new owner-issued pairing code.
    pub fn pair_device(
        &self,
        token: &str,
        installation: Uuid,
    ) -> Result<Option<(i64, String)>, DbError> {
        if !token.starts_with("csdp_") || installation.is_nil() {
            return Ok(None);
        }
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            let id: Option<i64> = tx.query_row("SELECT device_id FROM device_pairing_tokens WHERE token_hash=?1 AND expires_at>?2",params![hash_token(token),unix_now()],|r|r.get(0)).optional()?;
            let Some(id) = id else { return Ok(None) };
            let credential = format!("csdc_{}", super::auth::random_secret());
            tx.execute("INSERT INTO device_credentials(device_id,credential_hash,installation_id,paired_at) VALUES(?1,?2,?3,?4) ON CONFLICT(device_id) DO UPDATE SET credential_hash=excluded.credential_hash,installation_id=excluded.installation_id,paired_at=excluded.paired_at",params![id,hash_token(&credential),installation.to_string(),unix_now()])?;
            tx.execute("DELETE FROM device_pairing_tokens WHERE device_id=?1",[id])?;
            tx.commit()?;
            Ok(Some((id,credential)))
        })
    }

    pub fn authenticate_device(
        &self,
        credential: &str,
        installation: Uuid,
    ) -> Result<Option<i64>, DbError> {
        if !credential.starts_with("csdc_") {
            return Ok(None);
        }
        self.with_conn(|c| c.query_row("SELECT device_id FROM device_credentials WHERE credential_hash=?1 AND installation_id=?2",params![hash_token(credential),installation.to_string()],|r|r.get(0)).optional())
    }

    /// Also removes outstanding pairing codes so revocation cannot be undone by
    /// an old code. The owner must explicitly issue a new one.
    pub fn revoke_device(&self, id: i64) -> Result<(), DbError> {
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            tx.execute("DELETE FROM device_credentials WHERE device_id=?1", [id])?;
            tx.execute("DELETE FROM device_pairing_tokens WHERE device_id=?1", [id])?;
            tx.commit()
        })
    }

    pub fn device_channels(&self, device: i64) -> Result<Vec<DeviceChannel>, DbError> {
        self.with_conn(|c| c.prepare("SELECT id,device_id,guild_id,channel_id,channel_name FROM device_channels WHERE device_id=?1 ORDER BY id")?.query_map([device],|r|Ok(DeviceChannel {
            id:r.get(0)?,device_id:r.get(1)?,guild_id:super::discord_api::snowflake_string(r.get(2)?),channel_id:super::discord_api::snowflake_string(r.get(3)?),channel_name:r.get(4)?,
        }))?.collect())
    }

    pub fn add_device_channel(
        &self,
        device: i64,
        guild: i64,
        channel: i64,
        name: &str,
    ) -> Result<(), DbError> {
        self.with_conn(|c| c.execute("INSERT INTO device_channels(device_id,guild_id,channel_id,channel_name) VALUES(?1,?2,?3,?4) ON CONFLICT(device_id,channel_id) DO NOTHING",params![device,guild,channel,name]).map(|_|()))
    }

    pub fn delete_device_channel(&self, device: i64, route: i64) -> Result<(), DbError> {
        self.with_conn(|c| {
            c.execute(
                "DELETE FROM device_channels WHERE device_id=?1 AND id=?2",
                params![device, route],
            )
            .map(|_| ())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn fixture() -> (Db, Device) {
        let db = Db::open_in_memory().unwrap();
        db.with_conn(|c|c.execute("INSERT INTO users(discord_user_id,username,created_at,last_auth_at) VALUES(1,'owner',0,0)",[]).map(|_|())).unwrap();
        let d = db.create_device(1, "Pier", DeviceKind::PierCamera).unwrap();
        (db, d)
    }

    #[test]
    fn pairing_is_single_use_identity_bound_and_separate_from_rigs() {
        let (db, d) = fixture();
        let installation = Uuid::new_v4();
        let token = db.issue_device_token(d.id).unwrap();
        assert!(db.consume_pairing_token(&token).unwrap().is_none());
        let (id, key) = db.pair_device(&token, installation).unwrap().unwrap();
        assert_eq!(id, d.id);
        assert!(db.pair_device(&token, installation).unwrap().is_none());
        assert_eq!(
            db.authenticate_device(&key, installation).unwrap(),
            Some(id)
        );
        assert!(
            db.authenticate_device(&key, Uuid::new_v4())
                .unwrap()
                .is_none()
        );
        let replacement = db.issue_device_token(id).unwrap();
        db.revoke_device(id).unwrap();
        assert!(
            db.authenticate_device(&key, installation)
                .unwrap()
                .is_none()
        );
        assert!(
            db.pair_device(&replacement, installation)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn expiry_rotation_and_delete() {
        let (db, d) = fixture();
        let old = db.issue_device_token(d.id).unwrap();
        let current = db.issue_device_token(d.id).unwrap();
        let installation = Uuid::new_v4();
        assert!(db.pair_device(&old, installation).unwrap().is_none());
        db.with_conn(|c| {
            c.execute(
                "UPDATE device_pairing_tokens SET expires_at=?1",
                [unix_now()],
            )
            .map(|_| ())
        })
        .unwrap();
        assert!(db.pair_device(&current, installation).unwrap().is_none());
        let token = db.issue_device_token(d.id).unwrap();
        let (_, key) = db.pair_device(&token, installation).unwrap().unwrap();
        let token = db.issue_device_token(d.id).unwrap();
        let (_, new_key) = db.pair_device(&token, installation).unwrap().unwrap();
        assert!(
            db.authenticate_device(&key, installation)
                .unwrap()
                .is_none()
        );
        db.delete_device(d.id).unwrap();
        assert!(
            db.authenticate_device(&new_key, installation)
                .unwrap()
                .is_none()
        );
    }
}
