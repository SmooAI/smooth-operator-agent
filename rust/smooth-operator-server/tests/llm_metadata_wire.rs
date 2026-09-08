//! Wire regression for per-agent LLM-request metadata: drive the REAL
//! `LlmClient` (built by the engine from `AgentConfig`) against a local mock
//! that CAPTURES the outbound `/chat/completions` body, and assert the
//! turn's `request_metadata` reaches it verbatim as top-level `metadata`.
//!
//! This is the one link the unit tests can't reach: the engine's run loop
//! builds the concrete client via `AgentConfig::with_metadata`, which the
//! mock-provider unit tests bypass. `None` ⇒ no `metadata` key on the wire.

use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::oneshot;

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::LlmConfig;
use smooth_operator_server::runner::{self, TurnRequest};

/// Spawn a mock gateway that captures the FIRST request's JSON body (sent back
/// over `body_tx`) and answers every POST with a trivial one-token completion.
async fn spawn_capturing_mock(body_tx: oneshot::Sender<Value>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut body_tx = Some(body_tx);
        while let Ok((mut sock, _)) = listener.accept().await {
            // Read until we have the full headers + Content-Length body.
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            while let Ok(n) = sock.read(&mut tmp).await {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                    let content_len = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= pos + 4 + content_len {
                        if let Some(tx) = body_tx.take() {
                            let body = &buf[pos + 4..];
                            let _ = tx.send(serde_json::from_slice(body).unwrap_or(Value::Null));
                        }
                        break;
                    }
                }
            }
            let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                sse.len(),
                sse
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        }
    });
    format!("http://{addr}")
}

/// Run one turn with the given `request_metadata` and return the captured
/// outbound request body.
async fn capture_request_body(request_metadata: Option<serde_json::Map<String, Value>>) -> Value {
    let (body_tx, body_rx) = oneshot::channel();
    let url = spawn_capturing_mock(body_tx).await;

    let mut llm = LlmConfig::openrouter("test-key");
    llm.api_url = url;
    llm.model = "openai/gpt-oss-120b".into();

    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());
    let (tx, _rx) = unbounded_channel::<Value>();
    let _ = runner::run_streaming_turn(
        TurnRequest {
            demo_tools: false,
            storage,
            llm,
            max_iterations: 1,
            conversation_id: "conv-md-1",
            request_id: "req-md-1",
            user_message: "hi",
            model_max_output: None,
            access: AccessContext::anonymous(),
            llm_provider: None, // REAL client → hits the mock over HTTP
            executor: None,
            reranker: None,
            confirmation: None,
            interactions: None,
            tool_provider: None,
            system_prompt: None,
            org_id: None,
            gateway_key: None,
            user_token: None,
            workflow: None,
            judge: None,
            greeting_section: None,
            skill_section: None,
            tool_hooks: vec![],
            enabled_tools: None,
            auth_gate: None,
            tool_configs: None,
            extensions: None,
            images: vec![],
            files: vec![],
            request_metadata,
        },
        &tx,
    )
    .await;

    body_rx.await.expect("captured request body")
}

#[tokio::test]
async fn analyst_config_metadata_reaches_the_wire() {
    let tag = serde_json::json!({ "smooai_agent_slug": "observability-analyst" })
        .as_object()
        .unwrap()
        .clone();
    let body = capture_request_body(Some(tag)).await;
    assert_eq!(
        body["metadata"]["smooai_agent_slug"], "observability-analyst",
        "per-agent metadata must reach the outbound /chat/completions body: {body}"
    );
}

#[tokio::test]
async fn default_config_sends_no_metadata() {
    let body = capture_request_body(None).await;
    assert!(
        body.get("metadata").is_none(),
        "no request_metadata ⇒ no `metadata` key on the wire: {body}"
    );
}
