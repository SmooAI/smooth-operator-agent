//! Rich Interactions — the `choices` kind (a structured `AskUserQuestion`),
//! exercised end-to-end through the real runner + handler.
//!
//! - **Rich path** (session declared the `choice_chips` capability): the turn
//!   parks inside the `request_choices` raise, an `interaction_required` event
//!   surfaces (kind `choices`, the questions spec), an invalid
//!   `submit_interaction` (a label that isn't offered) gets `interaction_invalid`
//!   and LEAVES the turn parked, a valid selection resumes the raise with the
//!   canonical payload.
//! - **Conversational fallback** (no capability): the same raise returns the
//!   kind's enumerated directive immediately (no park); the model's generic
//!   `submit_interaction` tool call is validated server-side and returns the
//!   IDENTICAL canonical payload.
//!
//! Runs fully offline (`MockLlmClient` scripts the tool calls).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator::domain::{Session, SessionStatus};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::LlmConfig;

use smooth_operator::interaction::InteractionRegistry;
use smooth_operator_server::config::{ServerConfig, StorageBackend};
use smooth_operator_server::handler;
use smooth_operator_server::runner::{self, InteractionConfig, TurnRequest};
use smooth_operator_server::state::AppState;
use smooth_operator_server::state::PendingInteraction;

const SESSION_ID: &str = "sess-choices-1";
const CONVERSATION_ID: &str = "conv-choices-1";
const REQUEST_ID: &str = "req-choices-1";

fn mock_llm() -> LlmConfig {
    LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o")
}

fn config() -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1".into(),
        port: 0,
        gateway_url: "https://example.invalid/v1".into(),
        gateway_key: None,
        model: "claude-haiku-4-5".into(),
        seed_kb: false,
        max_iterations: 6,
        max_tokens: 128,
        storage: StorageBackend::Memory,
        widget_auth_strict: false,
        confirm_tools: Vec::new(),
        judge_model: "claude-haiku-4-5".to_string(),
    }
}

fn test_session() -> Session {
    let now = chrono::Utc::now();
    Session {
        session_id: SESSION_ID.to_string(),
        conversation_id: CONVERSATION_ID.to_string(),
        organization_id: "org".to_string(),
        agent_id: Some("agent".to_string()),
        agent_name: "Agent".to_string(),
        user_participant_id: "u".to_string(),
        agent_participant_id: "a".to_string(),
        thread_id: CONVERSATION_ID.to_string(),
        status: Some(SessionStatus::Active),
        token_count: Some(0),
        message_count: Some(0),
        metadata: None,
        created_at: Some(now),
        updated_at: Some(now),
        ended_at: None,
        last_activity_at: Some(now),
    }
}

/// The interactions wiring the WS handler builds, over a real `AppState`.
/// `choices` has no host attach effect, so the attach callback is a no-op.
fn interactions_for(state: &AppState, capabilities: &[&str]) -> InteractionConfig {
    InteractionConfig {
        session_id: SESSION_ID.to_string(),
        kinds: Arc::new(InteractionRegistry::default()),
        capabilities: capabilities.iter().map(|s| (*s).to_string()).collect(),
        register: {
            let state = state.clone();
            Arc::new(
                move |sid: &str, interaction_id: &str, kind: &str, spec: &Value, responder| {
                    state.register_interaction(
                        sid,
                        PendingInteraction {
                            interaction_id: interaction_id.to_string(),
                            kind: kind.to_string(),
                            spec: spec.clone(),
                            responder,
                        },
                    );
                },
            )
        },
        clear: {
            let state = state.clone();
            Arc::new(move |sid: &str| state.clear_interaction(sid))
        },
        attach: Arc::new(|_kind, _values| {}),
        persist: None,
    }
}

/// The two-question `request_choices` raise the mocks share (Plan single-select,
/// Topics multi-select).
const RAISE_ARGS: &str = r#"{"questions":[
    {"question":"Which plan interests you?","header":"Plan","options":[{"label":"Basic","description":"For individuals"},{"label":"Pro","description":"For teams"}]},
    {"question":"What can we help with?","header":"Topics","options":[{"label":"Sales"},{"label":"Support"}],"multiSelect":true}
],"reason":"to route you to the right team"}"#;

/// Turn-1 raises `request_choices`, then (turn-2) answers.
fn raising_mock() -> MockLlmClient {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "request_choices".into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk: RAISE_ARGS.into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ])
    .push_stream(vec![
        StreamEvent::Delta {
            content: "Great, routing you now.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);
    mock
}

fn spawn_turn(
    state: AppState,
    storage: Arc<dyn StorageAdapter>,
    mock: MockLlmClient,
    capabilities: &[&str],
    sink: UnboundedSender<Value>,
) -> tokio::task::JoinHandle<runner::TurnResult> {
    let interactions = interactions_for(&state, capabilities);
    tokio::spawn(async move {
        runner::run_streaming_turn(
            TurnRequest {
                demo_tools: false,
                storage,
                llm: mock_llm(),
                max_iterations: 6,
                conversation_id: CONVERSATION_ID,
                request_id: REQUEST_ID,
                user_message: "hi",
                model_max_output: None,
                access: AccessContext::anonymous(),
                llm_provider: Some(Arc::new(mock)),
                executor: None,
                reranker: None,
                confirmation: None,
                interactions: Some(interactions),
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

async fn await_event(rx: &mut UnboundedReceiver<Value>, wanted: &str) -> (Value, Vec<Value>) {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => {
                let hit = ev["type"] == wanted;
                seen.push(ev.clone());
                if hit {
                    return (ev, seen);
                }
            }
            Ok(None) => panic!("sink closed before '{wanted}'; saw: {seen:?}"),
            Err(_) => panic!("timed out waiting for '{wanted}'; saw: {seen:?}"),
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

async fn submit_frame(state: &AppState, sink: &UnboundedSender<Value>, body: Value) {
    handler::handle_frame(
        state,
        &AccessContext::anonymous(),
        "conn-1",
        None,
        None,
        &smooth_operator_server::handler::UserScope::Unscoped,
        &body.to_string(),
        sink,
    )
    .await;
}

#[tokio::test]
async fn rich_path_parks_validates_and_resumes_with_the_canonical_payload() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), config());
    state.insert_session(test_session());
    let (tx, mut rx) = unbounded_channel::<Value>();

    let turn = spawn_turn(
        state.clone(),
        storage as Arc<dyn StorageAdapter>,
        raising_mock(),
        &["choice_chips"],
        tx.clone(),
    );

    // 1. The turn parks and the spec-shaped event surfaces.
    let (pending, mut seen) = await_event(&mut rx, "interaction_required").await;
    assert_eq!(pending["requestId"], REQUEST_ID);
    let inner = &pending["data"]["data"];
    assert_eq!(inner["kind"], "choices");
    assert_eq!(inner["reason"], "to route you to the right team");
    assert_eq!(inner["spec"]["questions"][0]["header"], "Plan");
    assert_eq!(
        inner["spec"]["questions"][0]["options"][0]["label"],
        "Basic"
    );
    assert_eq!(inner["spec"]["questions"][1]["multiSelect"], true);
    let interaction_id = inner["interactionId"]
        .as_str()
        .expect("interactionId")
        .to_string();

    // 2. An INVALID submit (a label that isn't offered) → interaction_invalid,
    //    and the turn STAYS parked.
    submit_frame(
        &state,
        &tx,
        json!({
            "action": "submit_interaction",
            "requestId": REQUEST_ID,
            "sessionId": SESSION_ID,
            "interactionId": interaction_id,
            "kind": "choices",
            "values": { "answers": [
                { "header": "Plan", "options": ["Platinum"] },
                { "header": "Topics", "options": ["Sales"] }
            ] }
        }),
    )
    .await;
    let (invalid, _) = await_event(&mut rx, "interaction_invalid").await;
    assert_eq!(invalid["data"]["data"]["kind"], "choices");
    assert_eq!(invalid["data"]["data"]["errors"][0]["field"], "Plan");
    assert!(
        state.pending_interaction(SESSION_ID).is_some(),
        "invalid submit must leave the turn parked for a resubmit"
    );

    // 3. A VALID submit (single-select Plan + multi-select Topics + an 'Other')
    //    → ack + the parked raise resumes with canonical values.
    submit_frame(
        &state,
        &tx,
        json!({
            "action": "submit_interaction",
            "requestId": REQUEST_ID,
            "sessionId": SESSION_ID,
            "interactionId": interaction_id,
            "values": { "answers": [
                { "header": "Plan", "options": ["Pro"] },
                { "header": "Topics", "options": ["Sales"], "other": "Partnerships" }
            ] }
        }),
    )
    .await;

    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("turn should complete after submit")
        .expect("turn task");
    drain_into(&mut rx, &mut seen);

    let tool_text = tool_result_text(&seen);
    assert!(
        tool_text.contains(r#""status":"submitted""#),
        "tool result should be the canonical payload, got: {tool_text}"
    );
    assert!(
        tool_text.contains("Pro") && tool_text.contains("Partnerships"),
        "canonical answers reach the model: {tool_text}"
    );
    assert_eq!(result.reply, "Great, routing you now.");

    // The ack + park consumed.
    let acked = seen
        .iter()
        .any(|ev| ev["type"] == "immediate_response" && ev["message"] == "Interaction submitted");
    assert!(acked, "valid submit is acked: {seen:?}");
    assert!(
        state.pending_interaction(SESSION_ID).is_none(),
        "park consumed"
    );
}

#[tokio::test]
async fn text_channel_degrades_to_validated_conversational_collection() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), config());
    state.insert_session(test_session());
    let (tx, mut rx) = unbounded_channel::<Value>();

    // Scripted conversation: raise → (directive) → submit good answers →
    // (payload) → final answer.
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "request_choices".into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk: RAISE_ARGS.into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ])
    .push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_2".into(),
            name: "submit_interaction".into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk: r#"{"kind":"choices","values":{"answers":[{"header":"Plan","options":["Basic"]},{"header":"Topics","options":["Support","Sales"]}]}}"#.into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ])
    .push_stream(vec![
        StreamEvent::Delta {
            content: "All set.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);

    let turn = spawn_turn(
        state.clone(),
        storage as Arc<dyn StorageAdapter>,
        mock,
        &[], // no capabilities → conversational fallback for every kind
        tx,
    );

    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("turn completes without any park")
        .expect("turn task");

    let mut seen = Vec::new();
    drain_into(&mut rx, &mut seen);

    // No form/card event on a text channel.
    assert!(
        seen.iter().all(|ev| ev["type"] != "interaction_required"),
        "text channels must not receive the card event: {seen:?}"
    );

    let tool_text = tool_result_text(&seen);
    // 1. The raise degraded to the enumerated conversational directive (the
    //    core mirror truncates long tool results, so assert on its leading
    //    content, not the trailing `submit_interaction` instruction).
    assert!(
        tool_text.contains("cannot display choice chips"),
        "directive returned: {tool_text}"
    );
    // 2. The good submit produced the SAME validated payload as the card path.
    assert!(
        tool_text.contains(r#""status":"submitted""#),
        "conversational submit produces the canonical payload: {tool_text}"
    );
    assert!(tool_text.contains("Basic"));
    assert_eq!(result.reply, "All set.");
}
