//! WebSocket agent chat handler.
//!
//! Protocol:
//! ```text
//! Client -> Server: {"type":"message","content":"Hello"}
//! Client -> Server: {"type":"cancel"}
//! Server -> Client: {"type":"chunk","content":"Hi! "}
//! Server -> Client: {"type":"tool_call","name":"shell","args":{...}}
//! Server -> Client: {"type":"tool_result","name":"shell","output":"..."}
//! Server -> Client: {"type":"done","full_response":"..."}
//! Server -> Client: {"type":"cancelled"}
//! ```

use super::AppState;
use axum::{
    extract::{
        ws::{Message, WebSocket},
        Query, State, WebSocketUpgrade,
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
pub struct WsQuery {
    pub token: Option<String>,
}

/// GET /ws/chat — WebSocket upgrade for agent chat
pub async fn handle_ws_chat(
    State(state): State<AppState>,
    Query(params): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Auth via query param (browser WebSocket limitation)
    if state.pairing.require_pairing() {
        let token = params.token.as_deref().unwrap_or("");
        if !state.pairing.is_authenticated(token) {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                "Unauthorized — provide ?token=<bearer_token>",
            )
                .into_response();
        }
    }

    ws.on_upgrade(move |socket| handle_socket(socket, state))
        .into_response()
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();

    // Channel for forwarding non-cancel messages from the receiver task
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<String>(16);
    // Cancellation token shared between receiver listener and agent task
    let cancel = CancellationToken::new();

    // Spawn a task that continuously reads from the WebSocket receiver.
    // This keeps reading even while the agent is processing, so we can
    // detect cancel requests mid-flight.
    let cancel_clone = cancel.clone();
    let recv_handle = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            let text = match msg {
                Ok(Message::Text(t)) => t,
                Ok(Message::Close(_)) => break,
                Err(_) => break,
                _ => continue,
            };

            let parsed: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };

            match parsed["type"].as_str().unwrap_or("") {
                "cancel" => {
                    cancel_clone.cancel();
                }
                "message" => {
                    if msg_tx.send(text.to_string()).await.is_err() {
                        break;
                    }
                }
                _ => {}
            }
        }
    });

    // Main loop: wait for incoming messages and process them
    loop {
        let content = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            msg = msg_rx.recv() => {
                match msg {
                    Some(raw) => {
                        let parsed: serde_json::Value =
                            match serde_json::from_str(&raw) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                        let c = parsed["content"].as_str().unwrap_or("").to_string();
                        if c.is_empty() { continue; }
                        c
                    }
                    None => break, // receiver task ended
                }
            }
        };

        super::gateway_file_log(&format!("WS_REQUEST  message={content}"));

        let start = std::time::Instant::now();

        let provider_label = state
            .config
            .lock()
            .default_provider
            .clone()
            .unwrap_or_else(|| "unknown".to_string());

        // Broadcast agent_start event
        let _ = state.event_tx.send(serde_json::json!({
            "type": "agent_start",
            "provider": provider_label,
            "model": state.model,
        }));

        // Run agent with cancellation support
        let cancel_token = cancel.clone();
        let agent_fut = super::run_gateway_chat_with_tools(&state, &content);

        tokio::select! {
            biased;
            _ = cancel_token.cancelled() => {
                let elapsed = start.elapsed();
                super::gateway_file_log(&format!(
                    "WS_RESPONSE status=cancelled duration={:.2}s",
                    elapsed.as_secs_f64(),
                ));

                let msg = serde_json::json!({"type": "cancelled"});
                let _ = sender.send(Message::Text(msg.to_string().into())).await;

                let _ = state.event_tx.send(serde_json::json!({
                    "type": "agent_cancelled",
                    "provider": provider_label,
                    "model": state.model,
                }));

                break;
            }
            result = agent_fut => {
                match result {
                    Ok(response) => {
                        let done = serde_json::json!({
                            "type": "done",
                            "full_response": response,
                        });
                        let _ = sender.send(Message::Text(done.to_string().into())).await;

                        let elapsed = start.elapsed();
                        super::gateway_file_log(&format!(
                            "WS_RESPONSE status=200 duration={:.2}s model={} response={}",
                            elapsed.as_secs_f64(),
                            state.model,
                            response
                        ));

                        let _ = state.event_tx.send(serde_json::json!({
                            "type": "agent_end",
                            "provider": provider_label,
                            "model": state.model,
                        }));
                    }
                    Err(e) => {
                        let sanitized = crate::providers::sanitize_api_error(&e.to_string());
                        let err = serde_json::json!({
                            "type": "error",
                            "message": sanitized,
                        });
                        let _ = sender.send(Message::Text(err.to_string().into())).await;

                        let elapsed = start.elapsed();
                        super::gateway_file_log(&format!(
                            "WS_RESPONSE status=500 duration={:.2}s error={}",
                            elapsed.as_secs_f64(),
                            sanitized
                        ));

                        let _ = state.event_tx.send(serde_json::json!({
                            "type": "error",
                            "component": "ws_chat",
                            "message": sanitized,
                        }));
                    }
                }
            }
        }
    }

    recv_handle.abort();
}
