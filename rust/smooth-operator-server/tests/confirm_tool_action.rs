//! Write-confirmation HITL — the pause → `confirm_tool_action` → resume path.
//!
//! Proves the runner parks a turn that calls a confirmation-gated tool, surfaces
//! a `write_confirmation_required` event (per spec) carrying the pending action,
//! registers a resumable responder on `AppState`, and resumes (execute on
//! approve, skip with a rejection result on reject) when a `confirm_tool_action`
//! frame arrives.
//!
//! Runs fully offline: a `MockLlmClient` scripts the gated tool call so there is
//! no network / gateway key. We gate the always-registered `knowledge_search`
//! tool (via `confirm_tools`) so a real registered tool exercises the full
//! pause/resume seam without inventing a test-only write tool.
//!
//! The two assertions that matter:
//!   - **Approved** → the parked tool runs; its result reaches the model (a
//!     `stream_chunk` with the tool result) and the turn completes.
//!   - **Rejected** → the tool is blocked; the model sees a `blocked by hook`
//!     rejection result instead, and the turn still completes (no hang).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::{Document, DocumentType, LlmConfig};

use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::runner::{self, ConfirmationConfig, TurnRequest};
use smooth_operator_server::state::AppState;

const SESSION_ID: &str = "sess-hitl-1";
const CONVERSATION_ID: &str = "conv-hitl-1";
const REQUEST_ID: &str = "req-hitl-1";

/// Throwaway LLM config — the mock provider answers, this is never dialed.
fn mock_llm() -> LlmConfig {
    LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o")
}

/// A config that gates `knowledge_search` behind human confirmation.
fn confirm_config() -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1".into(),
        port: 0,
        gateway_url: "https://example.invalid/v1".into(),
        gateway_key: None,
        model: "claude-haiku-4-5".into(),
        seed_kb: false,
        max_iterations: 4,
        max_tokens: 128,
        storage: StorageBackend::Memory,
        widget_auth_strict: false,
        confirm_tools: vec!["knowledge_search".into()],
        judge_model: "claude-haiku-4-5".to_string(),
    }
}

/// Seed one public doc so `knowledge_search("alpha")` has something to return
/// when the call is approved.
fn seeded_storage() -> Arc<InMemoryStorageAdapter> {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let kb = storage.knowledge();
    let mut doc = Document::new(
        "The alpha office hours are open to the whole organization.",
        "handbook/hours.md",
        DocumentType::Documentation,
    );
    doc.id = "doc-public".to_string();
    kb.ingest(doc).expect("ingest public doc");
    storage
}

/// A mock LLM that turn-1 streams a `knowledge_search("alpha")` call, turn-2
/// streams the final answer (so the gated tool path is forced).
fn scripted_mock() -> MockLlmClient {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "knowledge_search".into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk: r#"{"query":"alpha"}"#.into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ])
    .push_stream(vec![
        StreamEvent::Delta {
            content: "Here is what I found.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);
    mock
}

/// Build the `ConfirmationConfig` over a real `AppState` so the test exercises
/// the actual `register_confirmation` / `clear_confirmation` registry.
fn confirmation_for(state: &AppState) -> ConfirmationConfig {
    ConfirmationConfig {
        host_approver: None,
        tool_patterns: vec!["knowledge_search".into()],
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

/// Spawn a turn (it parks on the gated tool) and return the join handle plus the
/// event receiver. The turn runs in the background so the test can observe the
/// `confirm_tool_action_required` event and then resume it.
fn spawn_turn(
    state: AppState,
    storage: Arc<dyn StorageAdapter>,
    mock: MockLlmClient,
    sink: UnboundedSender<Value>,
) -> tokio::task::JoinHandle<runner::TurnResult> {
    tokio::spawn(async move {
        runner::run_streaming_turn(
            TurnRequest {
                demo_tools: false,
                storage,
                llm: mock_llm(),
                max_iterations: 4,
                conversation_id: CONVERSATION_ID,
                request_id: REQUEST_ID,
                user_message: "Tell me about alpha",
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
            },
            &sink,
        )
        .await
        .expect("run_streaming_turn")
    })
}

/// Poll the sink (bounded) until a `write_confirmation_required` event arrives,
/// collecting every event seen along the way. Fails the test on timeout.
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

/// Drain whatever is queued (after the turn finished) into `seen`.
fn drain_into(rx: &mut UnboundedReceiver<Value>, seen: &mut Vec<Value>) {
    while let Ok(ev) = rx.try_recv() {
        seen.push(ev);
    }
}

/// Concatenate every tool-result string the model would read from `stream_chunk`s.
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

#[tokio::test]
async fn approved_confirmation_runs_the_gated_tool_and_completes() {
    let storage = seeded_storage();
    let state = AppState::new(storage.clone(), confirm_config());
    let (tx, mut rx) = unbounded_channel::<Value>();

    let turn = spawn_turn(
        state.clone(),
        storage as Arc<dyn StorageAdapter>,
        scripted_mock(),
        tx,
    );

    // 1. The turn parks: a `write_confirmation_required` event must surface (per
    //    spec), carrying the requestId + the tool name as the opaque toolId + a
    //    human-readable action description.
    let (pending, mut seen) = await_pending_action(&mut rx).await;
    assert_eq!(pending["requestId"], REQUEST_ID);
    assert_eq!(pending["data"]["requestId"], REQUEST_ID);
    let inner = &pending["data"]["data"];
    assert_eq!(inner["toolId"], "knowledge_search");
    assert!(
        inner["actionDescription"]
            .as_str()
            .unwrap_or_default()
            .contains("knowledge_search"),
        "actionDescription should name the tool: {inner}"
    );

    // 2. The responder must be registered on AppState (the resume seam).
    let responder = state
        .take_confirmation(SESSION_ID)
        .expect("a responder must be registered while the turn is parked");

    // 3. Approve → the parked tool runs.
    responder
        .send(smooth_operator_core::HumanResponse::Approved)
        .expect("send approval");

    // 4. The turn completes and the gated tool actually ran (its real KB result —
    //    not a "blocked by hook" message — reached the model).
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("turn should complete after approval")
        .expect("turn task");
    drain_into(&mut rx, &mut seen);

    let tool_text = tool_result_text(&seen);
    assert!(
        tool_text.contains("alpha office hours"),
        "approved tool result should contain the KB content, got: {tool_text}"
    );
    assert!(
        !tool_text.contains("blocked by hook"),
        "an approved tool must NOT be blocked, got: {tool_text}"
    );
    assert_eq!(result.reply, "Here is what I found.");

    // 5. The registration is cleared at turn end (we already took it; a second
    //    take is None regardless, but the clear path must not panic).
    assert!(state.take_confirmation(SESSION_ID).is_none());
}

#[tokio::test]
async fn rejected_confirmation_blocks_the_tool_but_turn_still_completes() {
    let storage = seeded_storage();
    let state = AppState::new(storage.clone(), confirm_config());
    let (tx, mut rx) = unbounded_channel::<Value>();

    let turn = spawn_turn(
        state.clone(),
        storage as Arc<dyn StorageAdapter>,
        scripted_mock(),
        tx,
    );

    let (_pending, mut seen) = await_pending_action(&mut rx).await;

    let responder = state
        .take_confirmation(SESSION_ID)
        .expect("a responder must be registered while the turn is parked");

    // Reject → the core ConfirmationHook blocks the tool; the model sees a
    // "blocked by hook: User denied" result instead of the KB content.
    responder
        .send(smooth_operator_core::HumanResponse::Denied {
            reason: "user rejected the action".into(),
        })
        .expect("send rejection");

    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("turn should complete after rejection (no hang)")
        .expect("turn task");
    drain_into(&mut rx, &mut seen);

    let tool_text = tool_result_text(&seen);
    assert!(
        tool_text.contains("blocked by hook") || tool_text.contains("User denied"),
        "rejected tool result should carry the block/denial, got: {tool_text}"
    );
    assert!(
        !tool_text.contains("alpha office hours"),
        "a rejected tool must NOT leak the KB content, got: {tool_text}"
    );
    // Turn still finishes cleanly with the model's wrap-up.
    assert_eq!(result.reply, "Here is what I found.");
}

#[tokio::test]
async fn confirm_tool_action_handler_routes_the_verdict_to_the_parked_turn() {
    // Exercise the real `handle_frame` dispatch for `confirm_tool_action`: a turn
    // parks, a frame approves it, and the handler acks + resumes.
    let storage = seeded_storage();
    let state = AppState::new(storage.clone(), confirm_config());
    let (tx, mut rx) = unbounded_channel::<Value>();

    let turn = spawn_turn(
        state.clone(),
        storage as Arc<dyn StorageAdapter>,
        scripted_mock(),
        tx,
    );

    let (_pending, mut seen) = await_pending_action(&mut rx).await;

    // Register the session the way the live `create_conversation_session` /
    // `send_message` path does — `confirm_tool_action` now routes through the
    // ownership chokepoint, which resolves the session before it will touch the
    // park (th-1b7ed0). A parked turn always has a registered session in the
    // real server; the fixture spawns one directly, so it must say so.
    state.insert_session(smooth_operator::domain::Session {
        session_id: SESSION_ID.into(),
        conversation_id: CONVERSATION_ID.into(),
        organization_id: String::new(),
        agent_id: Some("agent-hitl".into()),
        agent_name: "smooth-agent".into(),
        user_participant_id: "part-user".into(),
        agent_participant_id: "part-agent".into(),
        thread_id: CONVERSATION_ID.into(),
        status: Some(smooth_operator::domain::SessionStatus::Active),
        token_count: Some(0),
        message_count: Some(0),
        metadata: None,
        created_at: None,
        updated_at: None,
        ended_at: None,
        last_activity_at: None,
    });

    // A separate "control" sink for the confirm frame's ack (mirrors the live
    // server, where the confirm arrives on the same connection's reader).
    let (ctrl_tx, mut ctrl_rx) = unbounded_channel::<Value>();
    let frame = json!({
        "action": "confirm_tool_action",
        "requestId": "confirm-1",
        "sessionId": SESSION_ID,
        "approved": true,
    })
    .to_string();

    smooth_operator_server::handler::handle_frame(
        &state,
        &AccessContext::anonymous(),
        "conn-1",
        None,
        None,
        &smooth_operator_server::handler::UserScope::Unscoped,
        &frame,
        &ctrl_tx,
    )
    .await;

    // The handler acks the confirmation.
    let ack = ctrl_rx.try_recv().expect("confirm ack");
    assert_eq!(ack["type"], "immediate_response");
    assert_eq!(ack["status"], 200);
    assert_eq!(ack["data"]["approved"], true);

    // And the parked turn resumed + completed with the tool run.
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("turn should complete after the confirm frame")
        .expect("turn task");
    drain_into(&mut rx, &mut seen);
    assert!(tool_result_text(&seen).contains("alpha office hours"));
    assert_eq!(result.reply, "Here is what I found.");

    // A duplicate confirm for the same session is now a clean no-op error.
    let (dup_tx, mut dup_rx) = unbounded_channel::<Value>();
    smooth_operator_server::handler::handle_frame(
        &state,
        &AccessContext::anonymous(),
        "conn-1",
        None,
        None,
        &smooth_operator_server::handler::UserScope::Unscoped,
        &frame,
        &dup_tx,
    )
    .await;
    let dup = dup_rx.try_recv().expect("dup confirm response");
    assert_eq!(dup["type"], "error");
    assert_eq!(dup["error"]["code"], "NO_PENDING_CONFIRMATION");
}
