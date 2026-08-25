use serde::{Deserialize, Serialize};

/// Transport-neutral response returned by a semantic N.I.N.A. command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CommandResponse {
    pub response: serde_json::Value,
    pub error: String,
    pub status_code: i32,
    pub success: bool,
    #[serde(rename = "Type")]
    pub response_type: String,
}

impl CommandResponse {
    /// A N.I.N.A. operation accepted for asynchronous execution has not
    /// succeeded yet. HTTP-style 202 is additive and remains understandable
    /// to Direct v1 peers that only know the existing response envelope.
    pub fn is_pending(&self) -> bool {
        self.success && self.status_code == 202
    }

    /// Best-effort human-readable summary of the response body.
    pub fn summary(&self) -> String {
        if !self.success {
            return if self.error.is_empty() {
                "failed".to_string()
            } else {
                format!("failed: {}", self.error)
            };
        }
        match &self.response {
            serde_json::Value::String(value) if !value.is_empty() => value.clone(),
            serde_json::Value::Bool(value) => value.to_string(),
            serde_json::Value::Null => "ok".to_string(),
            other => other.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_commands_are_not_reported_as_completed() {
        let accepted: CommandResponse = serde_json::from_value(serde_json::json!({
            "Response": "Sequence start requested",
            "Error": "",
            "StatusCode": 202,
            "Success": true,
            "Type": "API",
        }))
        .unwrap();
        assert!(accepted.is_pending());
        assert_eq!(accepted.summary(), "Sequence start requested");

        let completed = CommandResponse {
            status_code: 200,
            ..accepted.clone()
        };
        assert!(!completed.is_pending());

        let failed = CommandResponse {
            success: false,
            error: "Mount is not connected".to_string(),
            ..accepted
        };
        assert!(!failed.is_pending());
        assert_eq!(failed.summary(), "failed: Mount is not connected");
    }
}
