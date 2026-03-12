//! Gateway protocol types for Paperclip adapter compatibility.
//!
//! This module implements the gateway protocol expected by the Paperclip
//! `ironclaw-gateway` adapter:
//!
//! 1. Server sends `connect.challenge` event with nonce
//! 2. Client sends `req connect` with signed device auth
//! 3. Client sends `req agent` with message
//! 4. Client sends `req agent.wait` for completion
//! 5. Server streams `event agent` frames

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A request frame from the client.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum GatewayRequest {
    /// Request with method and params.
    #[serde(rename = "req")]
    Req {
        /// Unique request ID for correlation.
        id: String,
        /// Method name (connect, agent, agent.wait, etc.)
        method: String,
        /// Method-specific parameters.
        #[serde(default)]
        params: serde_json::Value,
    },
}

/// A response frame to the client.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum GatewayResponse {
    /// Successful response.
    #[serde(rename = "res")]
    Res {
        /// ID matching the request.
        id: String,
        /// Success flag.
        ok: bool,
        /// Response payload.
        #[serde(skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
        /// Error details if not ok.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<GatewayError>,
    },
}

/// Error details in a response.
#[derive(Debug, Clone, Serialize)]
pub struct GatewayError {
    /// Error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Human-readable message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Additional details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// An event frame pushed from server to client.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum GatewayEvent {
    /// Server-pushed event.
    #[serde(rename = "event")]
    Event {
        /// Event type (connect.challenge, agent, etc.)
        event: String,
        /// Event payload.
        #[serde(skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
        /// Sequence number for ordering.
        #[serde(skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
    },
}

impl GatewayEvent {
    /// Create a connect.challenge event with a nonce.
    pub fn connect_challenge(nonce: &str) -> Self {
        GatewayEvent::Event {
            event: "connect.challenge".to_string(),
            payload: Some(serde_json::json!({ "nonce": nonce })),
            seq: None,
        }
    }

    /// Create an agent event with streamed data.
    pub fn agent_event(stream: &str, data: serde_json::Value, seq: u64) -> Self {
        GatewayEvent::Event {
            event: "agent".to_string(),
            payload: Some(serde_json::json!({
                "stream": stream,
                "data": data,
            })),
            seq: Some(seq),
        }
    }

    /// Create an agent.run_id event.
    pub fn agent_run_id(run_id: &Uuid) -> Self {
        GatewayEvent::Event {
            event: "agent".to_string(),
            payload: Some(serde_json::json!({
                "stream": "run_id",
                "data": run_id.to_string(),
            })),
            seq: None,
        }
    }
}

impl GatewayResponse {
    /// Create a successful response.
    pub fn ok(id: String, payload: serde_json::Value) -> Self {
        GatewayResponse::Res {
            id,
            ok: true,
            payload: Some(payload),
            error: None,
        }
    }

    /// Create an error response.
    pub fn error(id: String, code: &str, message: &str) -> Self {
        GatewayResponse::Res {
            id,
            ok: false,
            payload: None,
            error: Some(GatewayError {
                code: Some(code.to_string()),
                message: Some(message.to_string()),
                details: None,
            }),
        }
    }

    /// Create an error response with details.
    pub fn error_with_details(
        id: String,
        code: &str,
        message: &str,
        details: serde_json::Value,
    ) -> Self {
        GatewayResponse::Res {
            id,
            ok: false,
            payload: None,
            error: Some(GatewayError {
                code: Some(code.to_string()),
                message: Some(message.to_string()),
                details: Some(details),
            }),
        }
    }
}

// --- Connect params parsing ---

/// Parsed connect parameters from the client.
#[allow(non_snake_case)]
#[derive(Debug, Clone, Deserialize)]
pub struct ConnectParams {
    /// Minimum protocol version.
    #[serde(default)]
    pub minProtocol: Option<u32>,
    /// Maximum protocol version.
    #[serde(default)]
    pub maxProtocol: Option<u32>,
    /// Client identification.
    #[serde(default)]
    pub client: Option<ClientInfo>,
    /// Authentication payload.
    #[serde(default)]
    pub auth: Option<AuthPayload>,
    /// Device authentication (signed).
    #[serde(default)]
    pub device: Option<DeviceAuth>,
}

/// Client identification.
#[allow(non_snake_case)]
#[derive(Debug, Clone, Deserialize)]
pub struct ClientInfo {
    pub id: Option<String>,
    pub mode: Option<String>,
    pub version: Option<String>,
    pub platform: Option<String>,
    pub deviceFamily: Option<String>,
}

/// Authentication payload (shared secret or token).
#[derive(Debug, Clone, Deserialize)]
pub struct AuthPayload {
    pub token: Option<String>,
    pub password: Option<String>,
}

/// Device authentication (Ed25519 signed).
#[allow(non_snake_case)]
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceAuth {
    pub id: Option<String>,
    #[serde(alias = "publicKey")]
    pub publicKeyRawBase64Url: Option<String>,
    pub signature: Option<String>,
    #[serde(alias = "signedAt")]
    pub signedAtMs: Option<i64>,
    pub nonce: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub role: Option<String>,
}

// --- Agent params parsing ---

/// Agent invocation parameters.
#[allow(non_snake_case)]
#[derive(Debug, Clone, Deserialize)]
pub struct AgentParams {
    /// The message to send to the agent.
    pub message: Option<String>,
    /// Idempotency key (run ID).
    #[serde(default)]
    pub idempotencyKey: Option<String>,
    /// Session key for routing.
    #[serde(default)]
    pub sessionKey: Option<String>,
    /// Agent ID override.
    #[serde(default)]
    pub agentId: Option<String>,
    /// Timeout in milliseconds.
    #[serde(default)]
    pub timeoutMs: Option<u64>,
}

/// Agent.wait parameters.
#[allow(non_snake_case)]
#[derive(Debug, Clone, Deserialize)]
pub struct AgentWaitParams {
    /// Run ID to wait for.
    pub runId: Option<String>,
    /// Timeout in milliseconds.
    #[serde(default)]
    pub timeoutMs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_gateway_request() {
        let json = r#"{"type":"req","id":"abc-123","method":"connect","params":{"minProtocol":3}}"#;
        let req: GatewayRequest = serde_json::from_str(json).unwrap();
        match req {
            GatewayRequest::Req { id, method, params } => {
                assert_eq!(id, "abc-123");
                assert_eq!(method, "connect");
                assert_eq!(params["minProtocol"], 3);
            }
        }
    }

    #[test]
    fn test_serialize_gateway_response_ok() {
        let res = GatewayResponse::ok("abc-123".to_string(), serde_json::json!({"status": "ok"}));
        let json = serde_json::to_string(&res).unwrap();
        assert!(json.contains(r#""type":"res""#));
        assert!(json.contains(r#""id":"abc-123""#));
        assert!(json.contains(r#""ok":true"#));
    }

    #[test]
    fn test_serialize_gateway_response_error() {
        let res = GatewayResponse::error("abc-123".to_string(), "unauthorized", "Invalid token");
        let json = serde_json::to_string(&res).unwrap();
        assert!(json.contains(r#""ok":false"#));
        assert!(json.contains(r#""code":"unauthorized""#));
    }

    #[test]
    fn test_serialize_connect_challenge() {
        let event = GatewayEvent::connect_challenge("nonce-123");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""event":"connect.challenge""#));
        assert!(json.contains(r#""nonce":"nonce-123""#));
    }

    #[test]
    fn test_parse_connect_params() {
        let json = r#"{"minProtocol":3,"maxProtocol":3,"client":{"id":"gateway-client","mode":"backend"},"auth":{"token":"secret"}}"#;
        let params: ConnectParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.minProtocol, Some(3));
        assert_eq!(params.client.as_ref().unwrap().id, Some("gateway-client".to_string()));
        assert_eq!(params.auth.as_ref().unwrap().token, Some("secret".to_string()));
    }

    #[test]
    fn test_parse_agent_params() {
        let json = r#"{"message":"hello","idempotencyKey":"run-123","sessionKey":"paperclip"}"#;
        let params: AgentParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.message, Some("hello".to_string()));
        assert_eq!(params.idempotencyKey, Some("run-123".to_string()));
        assert_eq!(params.sessionKey, Some("paperclip".to_string()));
    }
}
