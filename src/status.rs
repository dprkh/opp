use crate::VERSION;
use serde::Serialize;
use std::io;
use std::time::SystemTime;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

#[derive(Debug, Serialize)]
pub(crate) struct Status {
    pub(crate) schema_version: u8,
    pub(crate) running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) authorization: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) account_selector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) op_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) broker_version: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) authorized_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) hard_expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_probe_at: Option<String>,
}

impl Status {
    pub(crate) fn stopped() -> Self {
        Self {
            schema_version: 1,
            running: false,
            authorization: None,
            account_selector: None,
            op_path: None,
            broker_version: None,
            started_at: None,
            authorized_at: None,
            hard_expires_at: None,
            next_probe_at: None,
        }
    }

    pub(crate) fn running(account_selector: Option<String>, op_path: String) -> Self {
        Self {
            schema_version: 1,
            running: true,
            authorization: Some("reauthorization_required"),
            account_selector,
            op_path: Some(op_path),
            broker_version: Some(VERSION),
            started_at: None,
            authorized_at: None,
            hard_expires_at: None,
            next_probe_at: None,
        }
    }

    pub(crate) fn json(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(io::Error::other)
    }

    pub(crate) fn valid_json(json: &[u8]) -> bool {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(json) else {
            return false;
        };
        let Some(object) = value.as_object() else {
            return false;
        };
        object
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            == Some(1)
            && object
                .get("running")
                .is_some_and(serde_json::Value::is_boolean)
    }
}

pub(crate) fn format_time(value: SystemTime) -> io::Result<String> {
    OffsetDateTime::from(value)
        .format(&Rfc3339)
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::Status;

    #[test]
    fn stopped_status_is_exact_and_valid() {
        let json = Status::stopped().json().unwrap();
        assert_eq!(json, br#"{"schema_version":1,"running":false}"#);
        assert!(Status::valid_json(&json));
    }

    #[test]
    fn invalid_status_json_is_rejected() {
        assert!(!Status::valid_json(b"not-json"));
        assert!(!Status::valid_json(b"[]"));
        assert!(!Status::valid_json(
            br#"{"schema_version":2,"running":false}"#
        ));
        assert!(!Status::valid_json(
            br#"{"schema_version":1,"running":"false"}"#
        ));
    }
}
