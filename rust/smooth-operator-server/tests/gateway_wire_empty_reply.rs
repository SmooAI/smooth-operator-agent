//! Gateway-wire regression: drive the REAL `LlmClient` (SSE parser, accumulator,
//! translator, runner) against a local mock speaking the OpenAI-compatible
//! gateway wire format. Pins the actual gpt-oss/groq stream shape that made
//! `eventual_response` ship an empty reply, so a fix can't regress silently.
//! This is NOT a hand-scripted engine mock — it exercises the same bytes prod
//! parses.

use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::unbounded_channel;

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::LlmConfig;

use smooth_operator_server::runner::{self, TurnRequest};

/// Spawn a local HTTP server that answers every POST /chat/completions with the
/// given SSE `data:` chunk bodies (each is a JSON object string) then `[DONE]`.
/// Serves each incoming request (one per agent-loop iteration) the SAME script.
async fn spawn_sse_mock(chunks: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let chunks = chunks.clone();
            tokio::spawn(async move {
                // Drain the request (read until headers+body end; good enough — we
                // just need to consume so the client's write completes).
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let mut body = String::new();
                for c in &chunks {
                    body.push_str("data: ");
                    body.push_str(c);
                    body.push_str("\n\n");
                }
                body.push_str("data: [DONE]\n\n");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });
    format!("http://{addr}")
}

async fn run_against(chunks: Vec<String>) -> (String, Vec<String>, Vec<String>, Vec<String>) {
    let url = spawn_sse_mock(chunks).await;
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());
    let mut llm = LlmConfig::openrouter("test-key");
    llm.api_url = url;
    llm.model = "openai/gpt-oss-120b".into();

    let (tx, mut rx) = unbounded_channel::<Value>();
    let result = runner::run_streaming_turn(
        TurnRequest {
            demo_tools: false,
            storage,
            llm,
            max_iterations: 4,
            conversation_id: "conv-wire-1",
            request_id: "req-wire-1",
            user_message: "How mature are our processes?",
            model_max_output: None,
            access: AccessContext::anonymous(),
            llm_provider: None, // REAL client → hits the mock over HTTP
            executor: None,
            reranker: None,
            confirmation: None,
            interactions: None,
            tool_provider: None,
            tool_hooks: vec![],
            system_prompt: None,
            org_id: None,
            gateway_key: None,
            user_token: None,
            workflow: None,
            judge: None,
            greeting_section: None,
            skill_section: None,
            enabled_tools: None,
            auth_gate: None,
            tool_configs: None,
            extensions: None,
            images: vec![],
            files: vec![],
            request_metadata: None,
        },
        &tx,
    )
    .await
    .expect("run_streaming_turn");
    drop(tx);

    let mut tokens = Vec::new();
    let mut reasoning = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        let ty = ev["type"].as_str().unwrap_or("");
        let tok = ev["data"]["data"]["token"]
            .as_str()
            .or_else(|| ev["data"]["token"].as_str())
            .or_else(|| ev["token"].as_str());
        if let Some(t) = tok {
            if ty == "stream_token" {
                tokens.push(t.to_string());
            } else if ty == "stream_reasoning" {
                reasoning.push(t.to_string());
            }
        }
    }
    (
        result.reply,
        tokens,
        reasoning,
        result.suggested_next_actions,
    )
}

fn delta_content(s: &str) -> String {
    serde_json::json!({"choices":[{"index":0,"delta":{"content": s}}]}).to_string()
}
fn delta_reasoning(s: &str) -> String {
    serde_json::json!({"choices":[{"index":0,"delta":{"reasoning_content": s}}]}).to_string()
}
fn finish() -> String {
    serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}).to_string()
}

const TRAILER: &str = "\n<suggested_replies>[\"Tell me more\", \"That's all\"]</suggested_replies>";

#[tokio::test]
async fn scenario_normal_answer_in_content() {
    let mut chunks = vec![delta_reasoning("thinking about maturity...")];
    for w in ["Your ", "processes ", "are ", "repeatable."] {
        chunks.push(delta_content(w));
    }
    chunks.push(delta_content(TRAILER));
    chunks.push(finish());
    let (reply, tokens, _reasoning, sna) = run_against(chunks).await;
    assert_eq!(reply, "Your processes are repeatable.");
    // The trailing "\n" before the (suppressed) trailer streams as a token.
    assert_eq!(tokens.concat().trim_end(), "Your processes are repeatable.");
    assert!(
        !tokens.concat().contains("<suggested_replies>"),
        "trailer marker must not leak into the token stream"
    );
    assert_eq!(sna, vec!["Tell me more", "That's all"]);
}

#[tokio::test]
async fn scenario_answer_in_reasoning_channel() {
    // gpt-oss/groq quirk (CONFIRMED against the real SSE parser): the whole answer
    // (and trailer) arrives on the reasoning channel; `content` is never
    // populated. Pre-fix this shipped an EMPTY eventual_response; the reasoning
    // fallback recovers the answer + suggestions. (th-emptyreply2)
    let mut chunks = Vec::new();
    for w in ["Your ", "processes ", "are ", "repeatable."] {
        chunks.push(delta_reasoning(w));
    }
    chunks.push(delta_reasoning(TRAILER));
    chunks.push(finish());
    let (reply, _tokens, reasoning, sna) = run_against(chunks).await;
    assert_eq!(
        reply, "Your processes are repeatable.",
        "answer on the reasoning channel must still fill responseParts"
    );
    assert_eq!(
        sna,
        vec!["Tell me more", "That's all"],
        "suggestions parsed off the reasoning-channel trailer"
    );
    assert!(
        reasoning
            .concat()
            .contains("Your processes are repeatable."),
        "the answer streamed to the client as reasoning"
    );
}
