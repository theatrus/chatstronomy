//! Durable identities of autofocus reports successfully delivered to Discord.
//!
//! A plugin may replay its latest completion after reconnecting. In-memory
//! updater state cannot distinguish that replay after a Hub restart, so retain
//! delivery identities per telescope, N.I.N.A. profile, and destination. These
//! rows contain no report payload, location, or equipment data. They remain
//! until telescope deletion; expiring them could re-announce an old report.
//! Storage grows by one small row per completed report and destination.
//!
//! Record only after a successful destination send. This is restart-resistant
//! deduplication, not exactly-once delivery: a crash between Discord accepting
//! a message and committing its identity can still replay that message.

use super::db::{Db, DbError, unix_now};

impl Db {
    /// Has this destination already received this profile's normalized report
    /// identity? Callers must use the same normalization for lookup and record.
    pub fn autofocus_delivered(
        &self,
        telescope_id: i64,
        profile_id: &str,
        channel_id: i64,
        report_identity: &str,
    ) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT EXISTS (
                    SELECT 1 FROM autofocus_deliveries
                    WHERE telescope_id = ?1 AND profile_id = ?2
                      AND channel_id = ?3 AND report_identity = ?4
                )",
                rusqlite::params![telescope_id, profile_id, channel_id, report_identity],
                |row| row.get(0),
            )
        })
    }

    /// Record a confirmed send without changing its original delivery time on
    /// retries. If the route was removed or reassigned during the send, this is
    /// a no-op; a stale updater must not create state for a removed telescope.
    /// Existing history remains when routes are removed so reattaching one
    /// does not cause old reports to be posted again.
    pub fn record_autofocus_delivery(
        &self,
        telescope_id: i64,
        profile_id: &str,
        channel_id: i64,
        report_identity: &str,
    ) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO autofocus_deliveries
                    (telescope_id, profile_id, channel_id, report_identity, delivered_at)
                 SELECT ?1, ?2, ?3, ?4, ?5
                 WHERE EXISTS (
                     SELECT 1 FROM telescope_channels
                     WHERE telescope_id = ?1 AND channel_id = ?3
                 )
                 ON CONFLICT(telescope_id, profile_id, channel_id, report_identity)
                     DO NOTHING",
                rusqlite::params![
                    telescope_id,
                    profile_id,
                    channel_id,
                    report_identity,
                    unix_now()
                ],
            )
            .map(|_| ())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = "157ca680-43e3-4029-8d7c-b90f3e65a68c";
    const OTHER_PROFILE: &str = "773917ba-2f41-4327-b28d-a644aa03e3f1";
    const REPORT: &str = "2026-09-11T01:02:03.456Z";
    const OTHER_REPORT: &str = "2026-09-11T02:03:04.567Z";

    fn seed(db: &Db) {
        db.with_conn(|conn| {
            conn.execute_batch(
                "INSERT INTO users (discord_user_id, username, created_at, last_auth_at)
                     VALUES (1, 'owner', 0, 0);
                 INSERT INTO guilds (guild_id, name, registered_by, created_at, updated_at)
                     VALUES (10, 'guild', 1, 0, 0);
                 INSERT INTO telescopes (id, owner_id, name, created_at)
                     VALUES (1, 1, 'scope one', 0), (2, 1, 'scope two', 0);
                 INSERT INTO telescope_channels
                     (telescope_id, guild_id, channel_id, created_by, created_at)
                     VALUES (1, 10, 11, 1, 0), (1, 10, 12, 1, 0), (2, 10, 21, 1, 0);",
            )
        })
        .unwrap();
    }

    fn row_count(db: &Db) -> i64 {
        db.with_conn(|conn| {
            conn.query_row("SELECT COUNT(*) FROM autofocus_deliveries", [], |r| {
                r.get(0)
            })
        })
        .unwrap()
    }

    #[test]
    fn deliveries_are_isolated_by_telescope_profile_channel_and_report() {
        let db = Db::open_in_memory().unwrap();
        seed(&db);
        assert!(!db.autofocus_delivered(1, PROFILE, 11, REPORT).unwrap());
        db.record_autofocus_delivery(1, PROFILE, 11, REPORT)
            .unwrap();
        assert!(db.autofocus_delivered(1, PROFILE, 11, REPORT).unwrap());
        assert!(!db.autofocus_delivered(2, PROFILE, 11, REPORT).unwrap());
        assert!(
            !db.autofocus_delivered(1, OTHER_PROFILE, 11, REPORT)
                .unwrap()
        );
        assert!(!db.autofocus_delivered(1, PROFILE, 12, REPORT).unwrap());
        assert!(
            !db.autofocus_delivered(1, PROFILE, 11, OTHER_REPORT)
                .unwrap()
        );
        db.record_autofocus_delivery(1, OTHER_PROFILE, 11, REPORT)
            .unwrap();
        db.record_autofocus_delivery(1, PROFILE, 12, REPORT)
            .unwrap();
        db.record_autofocus_delivery(1, PROFILE, 11, OTHER_REPORT)
            .unwrap();
        db.record_autofocus_delivery(2, PROFILE, 21, REPORT)
            .unwrap();
        assert_eq!(row_count(&db), 5);
    }

    #[test]
    fn recording_twice_is_idempotent_and_preserves_original_delivery_time() {
        let db = Db::open_in_memory().unwrap();
        seed(&db);
        db.record_autofocus_delivery(1, PROFILE, 11, REPORT)
            .unwrap();
        db.with_conn(|conn| conn.execute("UPDATE autofocus_deliveries SET delivered_at = 123", []))
            .unwrap();
        db.record_autofocus_delivery(1, PROFILE, 11, REPORT)
            .unwrap();
        assert_eq!(row_count(&db), 1);
        let delivered_at: i64 = db
            .with_conn(|conn| {
                conn.query_row("SELECT delivered_at FROM autofocus_deliveries", [], |r| {
                    r.get(0)
                })
            })
            .unwrap();
        assert_eq!(delivered_at, 123);
    }

    #[test]
    fn deliveries_survive_database_reopen() {
        let path = std::env::temp_dir().join(format!(
            "chatstronomy-autofocus-delivery-{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        {
            let db = Db::open(&path).unwrap();
            seed(&db);
            db.record_autofocus_delivery(1, PROFILE, 11, REPORT)
                .unwrap();
        }
        {
            let db = Db::open(&path).unwrap();
            assert!(db.autofocus_delivered(1, PROFILE, 11, REPORT).unwrap());
            assert!(!db.autofocus_delivered(1, PROFILE, 12, REPORT).unwrap());
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn telescope_deletion_cascades_and_a_stale_updater_cannot_recreate_history() {
        let db = Db::open_in_memory().unwrap();
        seed(&db);
        db.record_autofocus_delivery(1, PROFILE, 11, REPORT)
            .unwrap();
        db.record_autofocus_delivery(2, PROFILE, 21, REPORT)
            .unwrap();
        db.delete_telescope(1).unwrap();
        assert!(!db.autofocus_delivered(1, PROFILE, 11, REPORT).unwrap());
        assert!(db.autofocus_delivered(2, PROFILE, 21, REPORT).unwrap());
        db.record_autofocus_delivery(1, PROFILE, 11, REPORT)
            .unwrap();
        assert_eq!(row_count(&db), 1);
    }

    #[test]
    fn removing_route_retains_history_but_prevents_new_records() {
        let db = Db::open_in_memory().unwrap();
        seed(&db);
        db.record_autofocus_delivery(1, PROFILE, 11, REPORT)
            .unwrap();
        db.with_conn(|conn| {
            conn.execute("DELETE FROM telescope_channels WHERE channel_id = 11", [])
        })
        .unwrap();
        assert!(db.autofocus_delivered(1, PROFILE, 11, REPORT).unwrap());
        db.record_autofocus_delivery(1, PROFILE, 11, OTHER_REPORT)
            .unwrap();
        assert!(
            !db.autofocus_delivered(1, PROFILE, 11, OTHER_REPORT)
                .unwrap()
        );
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO telescope_channels
                     (telescope_id, guild_id, channel_id, created_by, created_at)
                 VALUES (1, 10, 11, 1, 0)",
                [],
            )
        })
        .unwrap();
        assert!(db.autofocus_delivered(1, PROFILE, 11, REPORT).unwrap());
        db.record_autofocus_delivery(1, PROFILE, 11, OTHER_REPORT)
            .unwrap();
        assert_eq!(row_count(&db), 2);
    }

    #[test]
    fn reassigned_channel_has_independent_history_and_rejects_stale_records() {
        let db = Db::open_in_memory().unwrap();
        seed(&db);
        db.record_autofocus_delivery(1, PROFILE, 11, REPORT)
            .unwrap();
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE telescope_channels SET telescope_id = 2 WHERE channel_id = 11",
                [],
            )
        })
        .unwrap();
        assert!(!db.autofocus_delivered(2, PROFILE, 11, REPORT).unwrap());
        db.record_autofocus_delivery(1, PROFILE, 11, OTHER_REPORT)
            .unwrap();
        assert!(
            !db.autofocus_delivered(1, PROFILE, 11, OTHER_REPORT)
                .unwrap()
        );
        db.record_autofocus_delivery(2, PROFILE, 11, REPORT)
            .unwrap();
        assert!(db.autofocus_delivered(2, PROFILE, 11, REPORT).unwrap());
        assert_eq!(row_count(&db), 2);
    }
}
