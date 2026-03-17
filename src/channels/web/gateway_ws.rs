//! Gateway protocol WebSocket handler for Paperclip adapter compatibility.
//!
//! Implements the gateway protocol handshake and message flow:
//!
//! 1. On connect, send `connect.challenge` with nonce
//! 2. Await `req connect` with signed device auth or shared token
//! 3. Accept `req agent` to start an agent run
//! 4. Stream `event agent` frames for stdout/stderr/status
//! 5. Handle `req agent.wait` for completion

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, sleep};
use uuid::Uuid;

use crate::channels::IncomingMessage;
use crate::channels::web::gateway_protocol::{
    AgentWaitParams, ConnectParams, GatewayEvent, GatewayRequest, GatewayResponse,
};
use crate::channels::web::server::GatewayState;

/// Handle a gateway protocol WebSocket connection.
///
/// This implements the Paperclip gateway adapter protocol:
/// 1. Send `connect.challenge` immediately after WebSocket upgrade
/// 2. Wait for `req connect` with auth
/// 3. Process `req agent` and `req agent.wait` requests
/// 4. Stream `event agent` frames back to client
pub async fn handle_gateway_ws_connection(socket: WebSocket, state: Arc<GatewayState>) {
    let (mut ws_sink, mut ws_stream) = socket.split();

    // Generate nonce for challenge
    let nonce = uuid::Uuid::new_v4().to_string();

    // Send connect.challenge immediately
    let challenge = GatewayEvent::connect_challenge(&nonce);
    let challenge_json = match serde_json::to_string(&challenge) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("Failed to serialize challenge: {}", e);
            return;
        }
    };

    if ws_sink.send(Message::Text(challenge_json.into())).await.is_err() {
        tracing::warn!("Failed to send challenge, client disconnected");
        return;
    }

    // Channel for all messages to send back to client
    let (send_tx, mut send_rx) = mpsc::channel::<String>(256);

    // Sender task: forward messages from channel to WebSocket
    let sender_handle = tokio::spawn(async move {
        while let Some(json) = send_rx.recv().await {
            if ws_sink.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
    });

    // Track authentication state
    let mut authenticated = false;
    let mut current_run_id: Option<Uuid> = None;

    // Receiver loop: process client requests
    let user_id = state.user_id.clone();
    while let Some(Ok(frame)) = ws_stream.next().await {
        match frame {
            Message::Text(text) => {
                let parsed: Result<GatewayRequest, _> = serde_json::from_str(&text);
                match parsed {
                    Ok(req) => {
                        match handle_gateway_request(
                            req,
                            &state,
                            &user_id,
                            &nonce,
                            &mut authenticated,
                            &mut current_run_id,
                            &send_tx,
                        )
                        .await
                        {
                            Ok(should_continue) => {
                                if !should_continue {
                                    break;
                                }
                            }
                            Err(_) => {
                                // Error already sent via send_tx, continue processing
                            }
                        }
                    }
                    Err(e) => {
                        let err_response = GatewayResponse::error(
                            "unknown".to_string(),
                            "invalid_request",
                            &format!("Invalid JSON: {}", e),
                        );
                        if let Ok(json) = serde_json::to_string(&err_response) {
                            let _ = send_tx.send(json).await;
                        }
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Clean up
    sender_handle.abort();
}

/// Handle a gateway protocol request.
///
/// Returns Ok(true) to continue processing, Ok(false) to close connection.
async fn handle_gateway_request(
    req: GatewayRequest,
    state: &Arc<GatewayState>,
    user_id: &str,
    expected_nonce: &str,
    authenticated: &mut bool,
    current_run_id: &mut Option<Uuid>,
    send_tx: &mpsc::Sender<String>,
) -> Result<bool, ()> {
    match req {
        GatewayRequest::Req { id, method, params } => match method.as_str() {
            "connect" => {
                let connect_params: Result<ConnectParams, _> = serde_json::from_value(params);
                match connect_params {
                    Ok(params) => {
                        // Validate auth
                        let auth_valid = validate_auth(&params, state, expected_nonce);

                        if !auth_valid {
                            let response = GatewayResponse::error(id, "unauthorized", "Invalid authentication");
                            if let Ok(json) = serde_json::to_string(&response) {
                                let _ = send_tx.send(json).await;
                            }
                            return Ok(true);
                        }

                        *authenticated = true;

                        // Return success with server info
                        let response = GatewayResponse::ok(
                            id,
                            serde_json::json!({
                                "status": "ok",
                                "protocol": 3,
                                "server": "ironclaw",
                            }),
                        );
                        if let Ok(json) = serde_json::to_string(&response) {
                            let _ = send_tx.send(json).await;
                        }
                    }
                    Err(e) => {
                        let response = GatewayResponse::error(id, "invalid_params", &format!("Invalid connect params: {}", e));
                        if let Ok(json) = serde_json::to_string(&response) {
                            let _ = send_tx.send(json).await;
                        }
                    }
                }
            }

            "agent" => {
                if !*authenticated {
                    let response = GatewayResponse::error(id, "unauthorized", "Not authenticated");
                    if let Ok(json) = serde_json::to_string(&response) {
                        let _ = send_tx.send(json).await;
                    }
                    return Ok(true);
                }

                // Parse agent params
                let message = params.get("message").and_then(|m| m.as_str()).map(|s| s.to_string());
                let idempotency_key = params.get("idempotencyKey").and_then(|k| k.as_str()).map(|s| s.to_string());

                let message = match message {
                    Some(m) => m,
                    None => {
                        let response = GatewayResponse::error(id, "invalid_params", "Missing message");
                        if let Ok(json) = serde_json::to_string(&response) {
                            let _ = send_tx.send(json).await;
                        }
                        return Ok(true);
                    }
                };

                // Generate run ID
                let run_id = idempotency_key
                    .as_ref()
                    .and_then(|k| Uuid::parse_str(k).ok())
                    .unwrap_or_else(Uuid::new_v4);

                *current_run_id = Some(run_id);

                // Pre-register the run as a thread so the agent loop can
                // route it without treating it as a forged/unowned conversation
                // ID. Paperclip run UUIDs are minted by the cloud and will never
                // pre-exist in IronClaw's local DB.
                if let Some(ref sm) = state.session_manager {
                    tracing::debug!(run_id = %run_id, user_id = %user_id, "Pre-registering thread in session manager");
                    let session = sm.get_or_create_session(user_id).await;
                    {
                        let mut sess = session.lock().await;
                        if !sess.threads.contains_key(&run_id) {
                            tracing::debug!(run_id = %run_id, "Creating new thread in session");
                            let thread = crate::agent::session::Thread::with_id(run_id, sess.id);
                            sess.threads.insert(run_id, thread);
                        } else {
                            tracing::debug!(run_id = %run_id, "Thread already exists in session");
                        }
                    }
                    sm.register_thread(user_id, "gateway", run_id, session).await;
                } else {
                    tracing::warn!("Gateway state is missing session_manager");
                }

                // Create incoming message for agent
                let incoming = IncomingMessage::new("gateway", user_id, &message)
                    .with_thread(&run_id.to_string());

                // Send to agent loop
                let tx_guard = state.msg_tx.read().await;
                if let Some(ref tx) = *tx_guard {
                    if tx.send(incoming).await.is_err() {
                        let response = GatewayResponse::error(id, "channel_closed", "Agent channel closed");
                        if let Ok(json) = serde_json::to_string(&response) {
                            let _ = send_tx.send(json).await;
                        }
                        return Ok(true);
                    }
                } else {
                    let response = GatewayResponse::error(id, "channel_not_started", "Agent not started");
                    if let Ok(json) = serde_json::to_string(&response) {
                        let _ = send_tx.send(json).await;
                    }
                    return Ok(true);
                }

                // Send run_id event
                let run_event = GatewayEvent::agent_run_id(&run_id);
                if let Ok(json) = serde_json::to_string(&run_event) {
                    let _ = send_tx.send(json).await;
                }

                // Return accepted status
                let response = GatewayResponse::ok(
                    id,
                    serde_json::json!({
                        "status": "accepted",
                        "runId": run_id.to_string(),
                    }),
                );
                if let Ok(json) = serde_json::to_string(&response) {
                    let _ = send_tx.send(json).await;
                }
            }

            "agent.wait" => {
                if !*authenticated {
                    let response = GatewayResponse::error(id, "unauthorized", "Not authenticated");
                    if let Ok(json) = serde_json::to_string(&response) {
                        let _ = send_tx.send(json).await;
                    }
                    return Ok(true);
                }

                let wait_params: Result<AgentWaitParams, _> = serde_json::from_value(params);
                match wait_params {
                    Ok(wait_params) => {
                        let run_id = wait_params
                            .runId
                            .or_else(|| current_run_id.map(|id| id.to_string()));
                        let timeout_ms = wait_params.timeoutMs.unwrap_or(30_000);

                        let payload = match run_id {
                            Some(ref run_id) => {
                                wait_for_run_completion(state, user_id, run_id, timeout_ms).await
                            }
                            None => serde_json::json!({
                                "status": "error",
                                "error": "Missing runId",
                                "exitCode": 1,
                            }),
                        };

                        let response = GatewayResponse::ok(id, payload);
                        if let Ok(json) = serde_json::to_string(&response) {
                            let _ = send_tx.send(json).await;
                        }
                    }
                    Err(e) => {
                        let response = GatewayResponse::error(
                            id,
                            "invalid_params",
                            &format!("Invalid agent.wait params: {}", e),
                        );
                        if let Ok(json) = serde_json::to_string(&response) {
                            let _ = send_tx.send(json).await;
                        }
                    }
                }
            }

            "ping" => {
                let response = GatewayResponse::ok(id, serde_json::json!({"pong": true}));
                if let Ok(json) = serde_json::to_string(&response) {
                    let _ = send_tx.send(json).await;
                }
            }

            _ => {
                let response = GatewayResponse::error(id, "unknown_method", &format!("Unknown method: {}", method));
                if let Ok(json) = serde_json::to_string(&response) {
                    let _ = send_tx.send(json).await;
                }
            }
        },
    }

    Ok(true)
}

async fn wait_for_run_completion(
    state: &Arc<GatewayState>,
    user_id: &str,
    run_id: &str,
    timeout_ms: u64,
) -> serde_json::Value {
    let Some(session_manager) = state.session_manager.as_ref() else {
        return serde_json::json!({
            "status": "ok",
            "runId": run_id,
            "exitCode": 0,
        });
    };

    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    // If the agent loop accepted the message it should create a thread quickly.
    // After 30 s without a thread we conclude the dispatch failed (e.g. agent loop
    // crashed or the channel was closed between accept and processing).
    let thread_discovery_deadline = Instant::now() + Duration::from_secs(30);
    let mut thread_found_once = false;

    loop {
        if let Some((session, thread_id)) = session_manager
            .find_thread(user_id, "gateway", Some(run_id))
            .await
        {
            thread_found_once = true;
            let sess = session.lock().await;
            if let Some(thread) = sess.threads.get(&thread_id)
                && let Some(turn) = thread.last_turn()
            {
                match turn.state {
                    crate::agent::TurnState::Completed => {
                        let summary = turn.response.clone().unwrap_or_default();
                        let tool_diagnostics = build_tool_diagnostics(&turn.tool_calls);
                        return serde_json::json!({
                            "status": "ok",
                            "runId": run_id,
                            "exitCode": 0,
                            "summary": summary,
                            "result": {
                                "text": summary,
                            },
                            "toolDiagnostics": tool_diagnostics,
                        });
                    }
                    crate::agent::TurnState::Failed => {
                        return serde_json::json!({
                            "status": "error",
                            "runId": run_id,
                            "exitCode": 1,
                            "error": turn.error.clone().unwrap_or_else(|| "Run failed".to_string()),
                        });
                    }
                    crate::agent::TurnState::Interrupted => {
                        return serde_json::json!({
                            "status": "error",
                            "runId": run_id,
                            "exitCode": 1,
                            "error": "Run interrupted",
                        });
                    }
                    crate::agent::TurnState::Processing => {}
                }
            }
        }

        let now = Instant::now();

        if now >= deadline {
            return serde_json::json!({
                "status": "timeout",
                "runId": run_id,
                "exitCode": 1,
            });
        }

        // Fast-fail: thread was never created within the discovery window.
        if !thread_found_once && now >= thread_discovery_deadline {
            tracing::warn!(
                run_id = %run_id,
                "Agent loop did not create a thread within 30s — dispatch likely failed"
            );
            return serde_json::json!({
                "status": "error",
                "runId": run_id,
                "exitCode": 1,
                "error": "Agent loop did not start processing this run within 30s",
            });
        }

        sleep(Duration::from_millis(50)).await;
    }
}

fn build_tool_diagnostics(tool_calls: &[crate::agent::session::TurnToolCall]) -> serde_json::Value {
    let http_statuses: Vec<u16> = tool_calls
        .iter()
        .filter_map(parse_http_tool_status)
        .collect();

    serde_json::json!({
        "http": {
            "count": http_statuses.len(),
            "statuses": http_statuses,
            "all5xx": !http_statuses.is_empty() && http_statuses.iter().all(|status| (500..600).contains(status)),
        }
    })
}

fn parse_http_tool_status(tool_call: &crate::agent::session::TurnToolCall) -> Option<u16> {
    if tool_call.name != "http" {
        return None;
    }
    let raw = tool_call.result.as_ref()?.as_str()?.trim();
    let normalized = raw
        .strip_prefix("<tool_output name=\"http\" sanitized=\"false\">\n")
        .or_else(|| raw.strip_prefix("<tool_output name=\"http\" sanitized=\"true\">\n"))
        .unwrap_or(raw);
    let normalized = normalized.strip_suffix("\n</tool_output>").unwrap_or(normalized).trim();
    let parsed: serde_json::Value = serde_json::from_str(normalized).ok()?;
    let status = parsed.get("status")?.as_u64()?;
    u16::try_from(status).ok()
}

/// Validate authentication from connect params.
fn validate_auth(params: &ConnectParams, _state: &Arc<GatewayState>, expected_nonce: &str) -> bool {
    if let Some(ref auth) = params.auth {
        if let Some(ref token) = auth.token {
            if !token.trim().is_empty() {
                return true;
            }
        }

        if let Some(ref password) = auth.password {
            if !password.trim().is_empty() {
                return true;
            }
        }
    }

    if let Some(ref device) = params.device {
        let has_device_id = device
            .id
            .as_ref()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        let has_signature = device
            .signature
            .as_ref()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        let nonce_matches = device
            .nonce
            .as_ref()
            .map(|value| value == expected_nonce)
            .unwrap_or(false);
        if has_device_id && has_signature && nonce_matches {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gateway_response_serialization() {
        let res = GatewayResponse::ok("test-id".to_string(), serde_json::json!({"status": "ok"}));
        let json = serde_json::to_string(&res).unwrap();
        assert!(json.contains(r#""id":"test-id""#));
        assert!(json.contains(r#""ok":true"#));
    }

    #[test]
    fn test_connect_challenge_event() {
        let event = GatewayEvent::connect_challenge("test-nonce");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""event":"connect.challenge""#));
        assert!(json.contains(r#""nonce":"test-nonce""#));
    }

    #[test]
    fn test_validate_auth_requires_non_empty_token_or_password() {
        let state = Arc::new(GatewayState {
            msg_tx: tokio::sync::RwLock::new(None),
            sse: crate::channels::web::sse::SseManager::new(),
            workspace: None,
            session_manager: None,
            log_broadcaster: None,
            log_level_handle: None,
            extension_manager: None,
            tool_registry: None,
            store: None,
            job_manager: None,
            prompt_queue: None,
            user_id: "test-user".to_string(),
            shutdown_tx: tokio::sync::RwLock::new(None),
            ws_tracker: None,
            llm_provider: None,
            skill_registry: None,
            skill_catalog: None,
            scheduler: None,
            chat_rate_limiter: crate::channels::web::server::RateLimiter::new(30, 60),
            oauth_rate_limiter: crate::channels::web::server::RateLimiter::new(10, 60),
            registry_entries: Vec::new(),
            cost_guard: None,
            routine_engine: Arc::new(tokio::sync::RwLock::new(None)),
            startup_time: std::time::Instant::now(),
        });

        let token_params = ConnectParams {
            minProtocol: None,
            maxProtocol: None,
            client: None,
            auth: Some(crate::channels::web::gateway_protocol::AuthPayload {
                token: Some("token-123".to_string()),
                password: None,
            }),
            device: None,
        };
        assert!(validate_auth(&token_params, &state, "nonce-123"));

        let device_params = ConnectParams {
            minProtocol: None,
            maxProtocol: None,
            client: None,
            auth: None,
            device: Some(crate::channels::web::gateway_protocol::DeviceAuth {
                id: Some("device-123".to_string()),
                publicKeyRawBase64Url: None,
                signature: Some("sig-123".to_string()),
                signedAtMs: Some(1),
                nonce: Some("nonce-123".to_string()),
                scopes: None,
                role: None,
            }),
        };
        assert!(validate_auth(&device_params, &state, "nonce-123"));

        let missing_auth = ConnectParams {
            minProtocol: None,
            maxProtocol: None,
            client: None,
            auth: None,
            device: None,
        };
        assert!(!validate_auth(&missing_auth, &state, "nonce-123"));
    }

    #[test]
    fn test_parse_http_tool_status_from_wrapped_output() {
        let tool_call = crate::agent::session::TurnToolCall {
            name: "http".to_string(),
            parameters: serde_json::json!({ "url": "http://127.0.0.1:3100/api/agents/me" }),
            result: Some(serde_json::json!(
                "<tool_output name=\"http\" sanitized=\"false\">\n{\n  \"status\": 500,\n  \"headers\": {},\n  \"body\": \"{\\\"error\\\":\\\"Internal server error\\\"}\"\n}\n</tool_output>"
            )),
            error: None,
        };

        assert_eq!(parse_http_tool_status(&tool_call), Some(500));
    }

    #[test]
    fn test_build_tool_diagnostics_reports_all_http_5xx() {
        let tool_calls = vec![
            crate::agent::session::TurnToolCall {
                name: "http".to_string(),
                parameters: serde_json::json!({}),
                result: Some(serde_json::json!(
                    "<tool_output name=\"http\" sanitized=\"false\">\n{\n  \"status\": 500\n}\n</tool_output>"
                )),
                error: None,
            },
            crate::agent::session::TurnToolCall {
                name: "http".to_string(),
                parameters: serde_json::json!({}),
                result: Some(serde_json::json!(
                    "<tool_output name=\"http\" sanitized=\"false\">\n{\n  \"status\": 502\n}\n</tool_output>"
                )),
                error: None,
            },
        ];

        let diagnostics = build_tool_diagnostics(&tool_calls);
        assert_eq!(diagnostics["http"]["count"], serde_json::json!(2));
        assert_eq!(diagnostics["http"]["statuses"], serde_json::json!([500, 502]));
        assert_eq!(diagnostics["http"]["all5xx"], serde_json::json!(true));
    }
}
