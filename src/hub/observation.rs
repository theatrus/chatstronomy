//! Read-only overlay state. Never feeds action/image trigger admission.
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type ObservationQuery<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Vec<(i64, serde_json::Value, Option<Uuid>)>> + Send + 'a>,
>;

pub fn unavailable(telescope: i64) -> serde_json::Value {
    let now = Utc::now();
    serde_json::json!({"type":"observation_state", "telescope_id":telescope,
        "telescope_name":null, "state_epoch":null,"revision":null,
        "observed_at":now.to_rfc3339(),"valid_until":(now+Duration::seconds(30)).to_rfc3339(),
        "available":false,"target":null,"temperatures":{"focuser":null,"external":null},
        "completed_exposures":null})
}

pub async fn collect(
    state: &super::server::HubState,
    camera: i64,
) -> Vec<(i64, serde_json::Value, Option<Uuid>)> {
    let telescopes = state.db.observation_telescopes(camera).unwrap_or_default();
    futures_util::future::join_all(telescopes.into_iter().map(|(id, name)| async move {
        let Some(connection) = state
            .rig_connections
            .get(id)
            .filter(|c| c.capabilities.observation_state)
        else {
            return (id, unavailable(id), None);
        };
        let generation = Some(connection.connection_id);
        let result = connection
            .query(
                crate::direct::protocol::QueryKind::ObservationState,
                std::time::Duration::from_secs(2),
            )
            .await;
        let Ok(result) = result else {
            return (id, unavailable(id), generation);
        };
        let Ok(mut snapshot) = serde_json::from_value::<Snapshot>(result.payload) else {
            return (id, unavailable(id), generation);
        };
        if !result.ok || !snapshot.sanitize(Utc::now()) {
            return (id, unavailable(id), generation);
        }
        let available = snapshot.available;
        let valid_until = snapshot.observed_at + Duration::seconds(30);
        let mut message = serde_json::to_value(snapshot).expect("finite observation snapshot");
        message["type"] = "observation_state".into();
        message["telescope_id"] = id.into();
        message["telescope_name"] = if available {
            Some(
                name.chars()
                    .filter(|c| {
                        !c.is_control()
                            && !matches!(c,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')
                    })
                    .take(80)
                    .collect::<String>(),
            )
        } else {
            None
        }
        .into();
        message["valid_until"] = valid_until.to_rfc3339().into();
        (id, message, generation)
    }))
    .await
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Target {
    pub run_id: Uuid,
    pub name: String,
    pub started_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Temperature {
    pub celsius: f64,
    pub observed_at: DateTime<Utc>,
    pub valid_until: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Temperatures {
    pub focuser: Option<Temperature>,
    pub external: Option<Temperature>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExposureCount {
    pub count: u64,
    pub image_type: String,
    pub basis: String,
    pub as_of: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    pub state_epoch: Uuid,
    pub revision: u64,
    pub observed_at: DateTime<Utc>,
    pub available: bool,
    pub target: Option<Target>,
    pub temperatures: Temperatures,
    pub completed_exposures: Option<ExposureCount>,
}

impl Snapshot {
    pub fn sanitize(&mut self, now: DateTime<Utc>) -> bool {
        if self.state_epoch.is_nil()
            || self.revision > 9_007_199_254_740_991
            || self.observed_at < now - Duration::seconds(30)
            || self.observed_at > now + Duration::seconds(5)
        {
            return false;
        }
        if !self.available {
            self.target = None;
            self.temperatures = Temperatures::default();
            self.completed_exposures = None;
            return true;
        }
        self.target = self.target.take().and_then(|mut target| {
            target.name = target
                .name
                .chars()
                .filter(|c| {
                    !c.is_control()
                        && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
                })
                .take(120)
                .collect::<String>()
                .trim()
                .to_owned();
            (!target.run_id.is_nil()
                && !target.name.is_empty()
                && target.name != "Sequential Instruction Set"
                && target
                    .started_at
                    .is_none_or(|at| at <= self.observed_at + Duration::seconds(5)))
            .then_some(target)
        });
        for value in [
            &mut self.temperatures.focuser,
            &mut self.temperatures.external,
        ] {
            if value.as_ref().is_some_and(|v| {
                !v.celsius.is_finite()
                    || v.observed_at > self.observed_at + Duration::seconds(5)
                    || v.valid_until <= now
                    || v.valid_until < v.observed_at
                    || v.valid_until > v.observed_at + Duration::minutes(2)
            }) {
                *value = None;
            }
        }
        if self.target.is_none()
            || self.completed_exposures.as_ref().is_some_and(|v| {
                v.count > 9_007_199_254_740_991
                    || v.image_type != "LIGHT"
                    || v.basis != "successfully_saved"
                    || v.as_of > self.observed_at + Duration::seconds(5)
                    || self
                        .target
                        .as_ref()
                        .is_none_or(|t| t.started_at.is_none_or(|at| v.as_of < at))
            })
        {
            self.completed_exposures = None;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn fixture() -> Snapshot {
        let now = Utc::now();
        Snapshot {
            state_epoch: Uuid::new_v4(),
            revision: 42,
            observed_at: now,
            available: true,
            target: Some(Target {
                run_id: Uuid::new_v4(),
                name: "NGC 7023".into(),
                started_at: Some(now - Duration::hours(1)),
            }),
            temperatures: Temperatures {
                focuser: Some(Temperature {
                    celsius: 0.0,
                    observed_at: now,
                    valid_until: now + Duration::minutes(2),
                }),
                external: None,
            },
            completed_exposures: Some(ExposureCount {
                count: 505,
                image_type: "LIGHT".into(),
                basis: "successfully_saved".into(),
                as_of: now,
            }),
        }
    }
    #[test]
    fn state_keeps_explicit_unknowns_source_identity_and_zero() {
        let mut state = fixture();
        assert!(state.sanitize(Utc::now()));
        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value["completed_exposures"]["count"], 505);
        assert_eq!(value["temperatures"]["focuser"]["celsius"], 0.0);
        assert!(value["temperatures"]["external"].is_null());
        state.available = false;
        assert!(state.sanitize(Utc::now()));
        assert!(
            state.target.is_none()
                && state.completed_exposures.is_none()
                && state.temperatures.focuser.is_none()
        );
    }

    #[test]
    fn published_direct_fixture_matches_the_overlay_projection() {
        let envelope: crate::direct::protocol::DirectMessage = serde_json::from_str(include_str!(
            "../../contracts/direct/v1/fixtures/query-result-observation-state.json"
        ))
        .unwrap();
        let crate::direct::protocol::DirectMessage::QueryResult(result) = envelope else {
            panic!("query_result fixture");
        };
        let mut state: Snapshot = serde_json::from_value(result.payload).unwrap();
        assert!(state.sanitize(state.observed_at));
        assert_eq!(state.completed_exposures.unwrap().count, 505);
        let query: crate::direct::protocol::DirectMessage = serde_json::from_str(include_str!(
            "../../contracts/direct/v1/fixtures/query-observation-state.json"
        ))
        .unwrap();
        assert!(
            matches!(query,crate::direct::protocol::DirectMessage::Query(request) if request.kind==crate::direct::protocol::QueryKind::ObservationState)
        );
    }
    #[test]
    fn stale_invalid_or_incomplete_state_cannot_become_overlay_facts() {
        let mut state = fixture();
        state.observed_at -= Duration::seconds(31);
        assert!(!state.sanitize(Utc::now()));
        let mut state = fixture();
        state.temperatures.focuser.as_mut().unwrap().valid_until =
            Utc::now() - Duration::seconds(1);
        state.target.as_mut().unwrap().started_at = None;
        assert!(state.sanitize(Utc::now()));
        assert!(state.temperatures.focuser.is_none() && state.completed_exposures.is_none());
        let mut state = fixture();
        state.target = None;
        assert!(state.sanitize(Utc::now()));
        assert!(state.completed_exposures.is_none());
    }
}
