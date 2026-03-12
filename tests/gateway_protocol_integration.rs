use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use ironclaw::channels::IncomingMessage;
use ironclaw::channels::web::server::{start_server, GatewayState, RateLimiter};
use ironclaw::channels::web::sse::SseManager;
use ironclaw::channels::web::ws::WsConnectionTracker;

const AUTH_TOKEN: &str = "test-token-12345";
const TIMEOUT: Duration = Duration::from_secs(5);

async fn start_test_server() -> (
    SocketAddr,
    Arc<GatewayState>,
    mpsc::Receiver<IncomingMessage>,
) {
    let (agent_tx, agent_rx) = mpsc::channel(64);

    let state = Arc::new(GatewayState {
        msg_tx: tokio::sync::RwLock::new(Some(agent_tx)),
        sse: SseManager::new(),
        workspace: None,
        session_manager: None,
        log_broadcaster: None,
        log_level_handle: None,
        extension_manager: None,
        tool_registry: None,
        store: None,
        job_manager: None,
        prompt_queue: None,
        scheduler: None,
        user_id: "test-user".to_string(),
        shutdown_tx: tokio::sync::RwLock::new(None),
        ws_tracker: Some(Arc::new(WsConnectionTracker::new())),
        llm_provider: None,
        skill_registry: None,
        skill_catalog: None,
        chat_rate_limiter: RateLimiter::new(30, 60),
        registry_entries: Vec::new(),
        cost_guard: None,
        routine_engine: Arc::new(tokio::sync::RwLock::new(None)),
        startup_time: std::time::Instant::now(),
    });

    let bound_addr = start_server("127.0.0.1:0".parse().unwrap(), state.clone(), AUTH_TOKEN.to_string())
        .await
        .expect("failed to start server");

    (bound_addr, state, agent_rx)
}

async fn connect_gateway_ws(
    addr: SocketAddr,
    include_http_auth: bool,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let ws_scheme = "ws";
    let url = format!("{}://{}/api/gateway/ws", ws_scheme, addr);
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Origin",
        format!("http://127.0.0.1:{}", addr.port()).parse().unwrap(),
    );
    if include_http_auth {
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", AUTH_TOKEN).parse().unwrap(),
        );
    }
    let (stream, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("failed to connect websocket");
    stream
}

async fn recv_json(
    stream: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> serde_json::Value {
    let msg = timeout(TIMEOUT, stream.next())
        .await
        .expect("timed out waiting for websocket message")
        .expect("stream ended")
        .expect("websocket error");
    match msg {
        Message::Text(text) => serde_json::from_str(&text).expect("invalid json frame"),
        other => panic!("expected text frame, got {:?}", other),
    }
}

async fn expect_challenge(
    stream: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) -> String {
    let challenge = recv_json(stream).await;
    assert_eq!(challenge["type"], "event");
    assert_eq!(challenge["event"], "connect.challenge");
    challenge["payload"]["nonce"]
        .as_str()
        .expect("challenge nonce missing")
        .to_string()
}

async fn send_connect(
    stream: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    nonce: &str,
    with_protocol_auth: bool,
) -> serde_json::Value {
    let mut params = json!({
        "minProtocol": 3,
        "maxProtocol": 3,
        "client": {
            "id": "paperclip-test",
            "version": "test-suite",
            "platform": "linux",
            "mode": "backend"
        },
        "role": "operator",
        "scopes": ["operator.admin"]
    });
    if with_protocol_auth {
        params["auth"] = json!({ "token": AUTH_TOKEN });
    } else {
        params["device"] = json!({
            "id": "",
            "signature": "",
            "nonce": nonce,
        });
    }
    stream
        .send(Message::Text(
            json!({
                "type": "req",
                "id": "connect-1",
                "method": "connect",
                "params": params,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    recv_json(stream).await
}

#[tokio::test]
async fn test_gateway_connect_and_ping() {
    let (addr, _state, _agent_rx) = start_test_server().await;
    let mut ws = connect_gateway_ws(addr, true).await;

    let nonce = expect_challenge(&mut ws).await;
    let connected = send_connect(&mut ws, &nonce, true).await;
    assert_eq!(connected["type"], "res");
    assert_eq!(connected["ok"], true);
    assert_eq!(connected["payload"]["server"], "ironclaw");

    ws.send(Message::Text(
        json!({
            "type": "req",
            "id": "ping-1",
            "method": "ping",
            "params": {}
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let pong = recv_json(&mut ws).await;
    assert_eq!(pong["type"], "res");
    assert_eq!(pong["ok"], true);
    assert_eq!(pong["payload"]["pong"], true);

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_gateway_connect_requires_protocol_auth() {
    let (addr, _state, _agent_rx) = start_test_server().await;
    let mut ws = connect_gateway_ws(addr, true).await;

    let nonce = expect_challenge(&mut ws).await;
    let response = send_connect(&mut ws, &nonce, false).await;

    assert_eq!(response["type"], "res");
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "unauthorized");

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_gateway_agent_request_reaches_agent_loop_and_wait_succeeds() {
    let (addr, _state, mut agent_rx) = start_test_server().await;
    let mut ws = connect_gateway_ws(addr, true).await;

    let nonce = expect_challenge(&mut ws).await;
    let connected = send_connect(&mut ws, &nonce, true).await;
    assert_eq!(connected["ok"], true);

    let run_id = "4cc7e0d8-d32e-4c44-95fc-1f891f2bb8bb";
    ws.send(Message::Text(
        json!({
            "type": "req",
            "id": "agent-1",
            "method": "agent",
            "params": {
                "message": "hello from gateway",
                "idempotencyKey": run_id,
            }
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let first = recv_json(&mut ws).await;
    let second = recv_json(&mut ws).await;
    let response = if first["type"] == "res" { first } else { second };
    assert_eq!(response["ok"], true);
    assert_eq!(response["payload"]["status"], "accepted");
    assert_eq!(response["payload"]["runId"], run_id);

    let incoming = timeout(TIMEOUT, agent_rx.recv())
        .await
        .expect("timed out waiting for agent message")
        .expect("agent channel closed");
    assert_eq!(incoming.content, "hello from gateway");
    assert_eq!(incoming.thread_id.as_deref(), Some(run_id));
    assert_eq!(incoming.channel, "gateway");

    ws.send(Message::Text(
        json!({
            "type": "req",
            "id": "wait-1",
            "method": "agent.wait",
            "params": {
                "runId": run_id,
                "timeoutMs": 1000
            }
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let wait_response = recv_json(&mut ws).await;
    assert_eq!(wait_response["ok"], true);
    assert_eq!(wait_response["payload"]["exitCode"], 0);

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_gateway_websocket_requires_http_auth() {
    let (addr, _state, _agent_rx) = start_test_server().await;
    let ws_scheme = "ws";
    let url = format!("{}://{}/api/gateway/ws", ws_scheme, addr);
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Origin",
        format!("http://127.0.0.1:{}", addr.port()).parse().unwrap(),
    );
    let result = tokio_tungstenite::connect_async(request).await;
    assert!(result.is_err());
}
