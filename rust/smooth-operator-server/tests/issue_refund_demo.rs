//! The seeded-demo write-approval path: `issue_refund` is registered only in
//! demo mode and, when gated via `SMOOTH_AGENT_CONFIRM_TOOLS`, parks the turn for
//! human approval — proving the HITL demo gates a genuine **write** instead of a
//! read (`knowledge_search`).
//!
//! Runs fully offline: a `MockLlmClient` scripts the `issue_refund` call so there
//! is no network / gateway key. `demo_tools: true` on the `TurnRequest` is what
//! the seeded-demo server sets from `SMOOTH_AGENT_SEED_KB=1`.
//!
//! The absence case (default flavor never registers the tool) is proven by the
//! pure `demo_tools::register_demo_tools` unit test in the crate.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::LlmConfig;

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::runner::{self, ConfirmationConfig, TurnRequest};
use smooth_operator_server::state::AppState;

const SESSION_ID: &str = "sess-refund-1";
const CONVERSATION_ID: &str = "conv-refund-1";
const REQUEST_ID: &str = "req-refund-1";

/// Throwaway LLM config — the mock provider answers, this is never dialed.
fn mock_llm() -> LlmConfig {
    LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o")
}

/// A seeded-demo config that gates `issue_refund` (the WRITE) behind human
/// confirmation — exactly what `examples/*/docker-compose.yml` now set.
fn demo_config() -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1".into(),
        port: 0,
        gateway_url: "https://example.invalid/v1".into(),
        gateway_key: None,
        model: "claude-haiku-4-5".into(),
        seed_kb: true,
        max_iterations: 4,
        max_tokens: 128,
        storage: StorageBackend::Memory,
        widget_auth_strict: false,
        confirm_tools: vec!["issue_refund".into()],
        judge_model: "claude-haiku-4-5".to_string(),
    }
}

/// A mock LLM that turn-1 streams an `issue_refund` call, turn-2 streams the
/// final answer (so the gated write path is forced).
fn scripted_mock() -> MockLlmClient {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "issue_refund".into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk: r#"{"order_id":"ORD-1234"}"#.into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ])
    .push_stream(vec![
        StreamEvent::Delta {
            content: "Your refund is on its way.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);
    mock
}

fn confirmation_for(state: &AppState) -> ConfirmationConfig {
    ConfirmationConfig {
        host_approver: None,
        tool_patterns: vec!["issue_refund".into()],
        session_id: SESSION_ID.to_string(),
        register: {
            let state = state.clone();
            Arc::new(move |sid: &str, responder| state.register_confirmation(sid, responder))
        },
        clear: {
            let state = state.clone();
            Arc::new(move |sid: &str| state.clear_confirmation(sid))
        },
        persist: None,
        pre_approved: None,
    }
}

fn spawn_turn(
    state: AppState,
    storage: Arc<dyn StorageAdapter>,
    mock: MockLlmClient,
    sink: UnboundedSender<Value>,
) -> tokio::task::JoinHandle<runner::TurnResult> {
    tokio::spawn(async move {
        runner::run_streaming_turn(
            TurnRequest {
                storage,
                llm: mock_llm(),
                max_iterations: 4,
                conversation_id: CONVERSATION_ID,
                request_id: REQUEST_ID,
                user_message: "I want to return order ORD-1234 for a refund",
                model_max_output: None,
                access: AccessContext::anonymous(),
                llm_provider: Some(Arc::new(mock)),
                executor: None,
                reranker: None,
                confirmation: Some(confirmation_for(&state)),
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
                // Seeded-demo flavor: register the mock `issue_refund` write tool.
                demo_tools: true,
            },
            &sink,
        )
        .await
        .expect("run_streaming_turn")
    })
}

async fn await_pending_action(rx: &mut UnboundedReceiver<Value>) -> (Value, Vec<Value>) {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => {
                let is_pending = ev["type"] == "write_confirmation_required";
                seen.push(ev.clone());
                if is_pending {
                    return (ev, seen);
                }
            }
            Ok(None) => panic!("sink closed before a pending-action event; saw: {seen:?}"),
            Err(_) => panic!("timed out waiting for write_confirmation_required; saw: {seen:?}"),
        }
    }
}

fn drain_into(rx: &mut UnboundedReceiver<Value>, seen: &mut Vec<Value>) {
    while let Ok(ev) = rx.try_recv() {
        seen.push(ev);
    }
}

fn tool_result_text(events: &[Value]) -> String {
    let mut s = String::new();
    for ev in events {
        if let Some(result) = ev
            .pointer("/data/state/rawResponse/toolResult/result")
            .and_then(Value::as_str)
        {
            s.push_str(result);
            s.push('\n');
        }
    }
    s
}

/// The write tool is registered in demo mode AND parks for approval; on approval
/// the mock refund runs and its canned confirmation reaches the model.
#[tokio::test]
async fn issue_refund_is_registered_and_confirm_gated_in_demo_mode() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), demo_config());
    let (tx, mut rx) = unbounded_channel::<Value>();

    let turn = spawn_turn(
        state.clone(),
        storage as Arc<dyn StorageAdapter>,
        scripted_mock(),
        tx,
    );

    // 1. The write parks: a `write_confirmation_required` event surfaces naming
    //    `issue_refund` — the tool was registered (demo mode) AND gated.
    let (pending, mut seen) = await_pending_action(&mut rx).await;
    assert_eq!(pending["requestId"], REQUEST_ID);
    let inner = &pending["data"]["data"];
    assert_eq!(
        inner["toolId"], "issue_refund",
        "the parked write must be issue_refund, not a read: {inner}"
    );

    // 2. Approve → the parked write runs and returns its canned confirmation.
    let responder = state
        .take_confirmation(SESSION_ID)
        .expect("a responder must be registered while the write is parked");
    responder
        .send(smooth_operator_core::HumanResponse::Approved)
        .expect("send approval");

    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("turn should complete after approval")
        .expect("turn task");
    drain_into(&mut rx, &mut seen);

    let tool_text = tool_result_text(&seen);
    assert!(
        tool_text.contains("Refund issued for order ORD-1234"),
        "approved write should return the canned refund confirmation, got: {tool_text}"
    );
    assert!(
        !tool_text.contains("blocked by hook"),
        "an approved write must NOT be blocked, got: {tool_text}"
    );
    assert_eq!(result.reply, "Your refund is on its way.");
}
