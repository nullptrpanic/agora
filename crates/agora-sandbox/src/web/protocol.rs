use super::audit::TraceEvent;
use axum::extract::ws::Message;
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct AccessToken(String);

impl AccessToken {
    pub(super) fn generate() -> Self {
        Self(format!(
            "{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        ))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccessToken([redacted])")
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ClientControl {
    Auth { token: String },
    Resize { cols: u16, rows: u16 },
    Stop,
    Start,
    ClearTrace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SessionStatus {
    Idle,
    Starting,
    Running,
    Exited,
    Error,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ServerControl {
    Status {
        status: SessionStatus,
        exit_code: Option<i32>,
        message: Option<String>,
    },
    TraceBatch {
        events: Vec<TraceEvent>,
    },
    Snapshot {
        traces: Vec<TraceEvent>,
        diagnostics: Vec<String>,
        active_root_trace_id: Option<String>,
        terminal_truncated: bool,
        trace_truncated: bool,
        status: SessionStatus,
        exit_code: Option<i32>,
        message: Option<String>,
    },
    Diagnostic {
        message: String,
    },
    TraceCleared,
    ReplayStart {
        truncated: bool,
    },
    ReplayEnd,
}

#[derive(Debug)]
pub(super) struct ProtocolError(String);

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProtocolError {}

pub(super) fn parse_control(text: &str) -> Result<ClientControl, ProtocolError> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| ProtocolError(format!("invalid control message: {error}")))?;
    let control: ClientControl = serde_json::from_str(text)
        .map_err(|error| ProtocolError(format!("invalid control message: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| ProtocolError("control message must be an object".to_string()))?;
    let allowed = match &control {
        ClientControl::Auth { .. } => &["type", "token"][..],
        ClientControl::Resize { .. } => &["type", "cols", "rows"][..],
        ClientControl::Stop | ClientControl::Start | ClientControl::ClearTrace => &["type"][..],
    };
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(ProtocolError(format!(
            "control message contains unsupported field {field}"
        )));
    }
    if let ClientControl::Resize { cols, rows } = control
        && (!(2..=500).contains(&cols) || !(2..=500).contains(&rows))
    {
        return Err(ProtocolError(
            "terminal dimensions must be between 2 and 500".to_string(),
        ));
    }
    Ok(control)
}

pub(super) fn validate_auth(
    message: &Message,
    expected: &AccessToken,
) -> Result<(), ProtocolError> {
    let Message::Text(text) = message else {
        return Err(ProtocolError(
            "the first WebSocket message must authenticate".to_string(),
        ));
    };
    match parse_control(text)? {
        ClientControl::Auth { token } if token == expected.as_str() => Ok(()),
        ClientControl::Auth { .. } => Err(ProtocolError("invalid viewer token".to_string())),
        _ => Err(ProtocolError(
            "the first WebSocket message must authenticate".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::super::audit::{TraceEvent, TraceKind};
    use super::{
        AccessToken, ClientControl, ServerControl, SessionStatus, parse_control, validate_auth,
    };
    use axum::extract::ws::Message;
    use serde_json::json;

    #[test]
    fn access_token_debug_output_is_redacted() {
        let token = AccessToken::generate();

        assert_eq!(format!("{token:?}"), "AccessToken([redacted])");
        assert!(token.as_str().len() >= 48);
    }

    #[test]
    fn auth_must_be_the_first_matching_text_message() {
        let token = AccessToken::generate();
        let auth =
            Message::Text(format!(r#"{{"type":"auth","token":"{}"}}"#, token.as_str()).into());

        assert!(validate_auth(&auth, &token).is_ok());
        assert!(validate_auth(&Message::Binary(vec![1].into()), &token).is_err());
        assert!(
            validate_auth(
                &Message::Text(r#"{"type":"auth","token":"wrong"}"#.into()),
                &token
            )
            .is_err()
        );
    }

    #[test]
    fn resize_messages_enforce_terminal_bounds() {
        let valid = parse_control(r#"{"type":"resize","cols":120,"rows":40}"#).unwrap();
        assert!(matches!(
            valid,
            ClientControl::Resize {
                cols: 120,
                rows: 40
            }
        ));

        assert!(parse_control(r#"{"type":"resize","cols":1,"rows":40}"#).is_err());
        assert!(parse_control(r#"{"type":"resize","cols":120,"rows":501}"#).is_err());
    }

    #[test]
    fn protocol_accepts_only_the_fixed_control_surface() {
        assert!(matches!(
            parse_control(r#"{"type":"stop"}"#).unwrap(),
            ClientControl::Stop
        ));
        assert!(matches!(
            parse_control(r#"{"type":"start"}"#).unwrap(),
            ClientControl::Start
        ));
        assert!(matches!(
            parse_control(r#"{"type":"clear_trace"}"#).unwrap(),
            ClientControl::ClearTrace
        ));
        assert!(parse_control(r#"{"type":"run","command":"rm -rf /"}"#).is_err());
        assert!(parse_control(r#"{"type":"start","shell":"/bin/zsh"}"#).is_err());
    }

    #[test]
    fn server_protocol_carries_owned_status_trace_and_snapshot_messages() {
        let event = TraceEvent {
            id: 1,
            root_trace_id: "root".to_string(),
            kind: TraceKind::Exec,
            occurred_at: "now".to_string(),
            title: "/bin/echo hello".to_string(),
            detail: json!({ "pid": 42 }),
            source_bytes: 1,
        };
        let messages = [
            ServerControl::Status {
                status: SessionStatus::Running,
                exit_code: None,
                message: None,
            },
            ServerControl::TraceBatch {
                events: vec![event.clone()],
            },
            ServerControl::Snapshot {
                traces: vec![event],
                diagnostics: vec!["one malformed record".to_string()],
                active_root_trace_id: Some("root".to_string()),
                terminal_truncated: false,
                trace_truncated: true,
                status: SessionStatus::Running,
                exit_code: None,
                message: None,
            },
            ServerControl::TraceCleared,
        ];

        for message in messages {
            let encoded = serde_json::to_value(message).unwrap();
            assert!(encoded["type"].is_string());
        }
    }
}
