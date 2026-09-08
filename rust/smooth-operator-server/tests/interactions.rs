//! Rich Interactions — the extensible structured-interaction seam
//! (`docs/Architecture/Rich Interactions.md`), exercised through its first
//! kind, `identity_intake`.
//!
//! Proves both channel shapes end-to-end against the real runner + handler:
//!
//! - **Rich path** (session declared the kind's `identity_form` capability):
//!   the turn parks inside the raise tool, an `interaction_required` event
//!   surfaces (per spec, with `interactionId`/`kind`/`spec`), an invalid
//!   `submit_interaction` frame gets `interaction_invalid` and LEAVES the turn
//!   parked, a mismatched `interactionId` is rejected, a valid frame runs the
//!   kind's host effect (session identity attach) and resumes the raise with
//!   the canonical payload. A decline resumes with the declined payload.
//! - **Conversational fallback** (no capability): the same raise returns the
//!   kind's directive immediately (no park, no event); the model's generic
//!   `submit_interaction` tool call is validated server-side — a bad email is
//!   a tool error the model re-asks from, good values attach + return the
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

const SESSION_ID: &str = "sess-intake-1";
const CONVERSATION_ID: &str = "conv-intake-1";
const REQUEST_ID: &str = "req-intake-1";

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

/// A registered session so `attach_session_identity` has something to stamp.
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
        attach: {
            let state = state.clone();
            Arc::new(move |kind, values| {
                if kind == "identity_intake" {
                    if let Ok(v) =
                        serde_json::from_value::<smooth_operator::IntakeValues>(values.clone())
                    {
                        state.attach_session_identity(SESSION_ID, &v);
                    }
                }
            })
        },
        persist: None,
    }
}

/// A mock that turn-1 raises `request_identity_intake`/// A mock that turn-1 raises `request_identity_intake` (email required, name
/// optional), then answers.
fn raising_mock() -> MockLlmClient {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "request_identity_intake".into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk:
                r#"{"fields":[{"key":"email","required":true},{"key":"name","required":false}],"reason":"to send you the quote"}"#
                    .into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ])
    .push_stream(vec![
        StreamEvent::Delta {
            content: "Thanks, got it.".into(),
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
                user_message: "I'd like a quote",
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

/// Poll the sink until an event of `wanted` type arrives (bounded).
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

/// Every tool-result string the model read (from `stream_chunk`s).
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

/// Feed a `submit_identity_intake` frame through the real action dispatcher.
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
        &["identity_form"],
        tx.clone(),
    );

    // 1. The turn parks and the spec-shaped event surfaces.
    let (pending, mut seen) = await_event(&mut rx, "interaction_required").await;
    assert_eq!(pending["requestId"], REQUEST_ID);
    let inner = &pending["data"]["data"];
    assert_eq!(inner["kind"], "identity_intake");
    assert_eq!(inner["reason"], "to send you the quote");
    assert_eq!(inner["spec"]["fields"][0]["key"], "email");
    assert_eq!(inner["spec"]["fields"][0]["required"], true);
    let interaction_id = inner["interactionId"]
        .as_str()
        .expect("interactionId")
        .to_string();

    // 2. A submit with the WRONG interactionId is rejected and stays parked.
    submit_frame(
        &state,
        &tx,
        json!({
            "action": "submit_interaction",
            "requestId": REQUEST_ID,
            "sessionId": SESSION_ID,
            "interactionId": "stale-id",
            "values": { "email": "a@b.co" }
        }),
    )
    .await;
    let (mismatch, _) = await_event(&mut rx, "error").await;
    assert_eq!(mismatch["error"]["code"], "INTERACTION_MISMATCH");
    assert!(
        state.pending_interaction(SESSION_ID).is_some(),
        "still parked"
    );

    // 3. An INVALID submit → interaction_invalid, and the turn STAYS parked.
    submit_frame(
        &state,
        &tx,
        json!({
            "action": "submit_interaction",
            "requestId": REQUEST_ID,
            "sessionId": SESSION_ID,
            "interactionId": interaction_id,
            "kind": "identity_intake",
            "values": { "email": "not-an-email" }
        }),
    )
    .await;
    let (invalid, _) = await_event(&mut rx, "interaction_invalid").await;
    assert_eq!(invalid["data"]["data"]["kind"], "identity_intake");
    assert_eq!(invalid["data"]["data"]["interactionId"], interaction_id);
    assert_eq!(invalid["data"]["data"]["errors"][0]["field"], "email");
    assert!(
        state.pending_interaction(SESSION_ID).is_some(),
        "invalid submit must leave the turn parked for a resubmit"
    );

    // 4. A VALID submit → ack + the parked raise resumes with canonical values.
    submit_frame(
        &state,
        &tx,
        json!({
            "action": "submit_interaction",
            "requestId": REQUEST_ID,
            "sessionId": SESSION_ID,
            "interactionId": interaction_id,
            "values": { "email": "Alice@Example.COM", "name": "  Alice  " }
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
        tool_text.contains("Alice@example.com"),
        "email domain normalized: {tool_text}"
    );
    assert!(
        tool_text.contains(r#""name":"Alice""#),
        "name trimmed: {tool_text}"
    );
    assert_eq!(result.reply, "Thanks, got it.");

    // 5. The kind's host effect ran (the OTP-contact keys).
    let contact = state.session_contact(SESSION_ID);
    assert_eq!(contact.email.as_deref(), Some("Alice@example.com"));
    let session = state.get_session(SESSION_ID).unwrap();
    assert_eq!(
        session.metadata.as_ref().unwrap().get("userName").unwrap(),
        "Alice"
    );

    // 6. The ack + duplicate-submit no-op.
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
async fn rich_path_decline_resumes_gracefully() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), config());
    state.insert_session(test_session());
    let (tx, mut rx) = unbounded_channel::<Value>();

    let turn = spawn_turn(
        state.clone(),
        storage as Arc<dyn StorageAdapter>,
        raising_mock(),
        &["identity_form"],
        tx.clone(),
    );

    let (pending, mut seen) = await_event(&mut rx, "interaction_required").await;
    let interaction_id = pending["data"]["data"]["interactionId"].as_str().unwrap();
    submit_frame(
        &state,
        &tx,
        json!({
            "action": "submit_interaction",
            "requestId": REQUEST_ID,
            "sessionId": SESSION_ID,
            "interactionId": interaction_id,
            "declined": true
        }),
    )
    .await;

    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("turn completes after decline")
        .expect("turn task");
    drain_into(&mut rx, &mut seen);

    let tool_text = tool_result_text(&seen);
    assert!(
        tool_text.contains(r#""status":"declined""#),
        "declined payload reaches the model: {tool_text}"
    );
    assert_eq!(result.reply, "Thanks, got it.");
    // Nothing was attached.
    assert!(state.session_contact(SESSION_ID).is_empty());
}

#[tokio::test]
async fn text_channel_degrades_to_validated_conversational_collection() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), config());
    state.insert_session(test_session());
    let (tx, mut rx) = unbounded_channel::<Value>();

    // Scripted conversation: raise → (directive) → submit bad email → (tool
    // error) → submit good values → (payload) → final answer.
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "request_identity_intake".into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk: r#"{"fields":["email"],"reason":"to follow up"}"#.into(),
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
            arguments_chunk: r#"{"kind":"identity_intake","values":{"email":"nope"}}"#.into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ])
    .push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_3".into(),
            name: "submit_interaction".into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk: r#"{"kind":"identity_intake","values":{"email":"bob@example.com","phone":"555-123-4567"}}"#.into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ])
    .push_stream(vec![
        StreamEvent::Delta {
            content: "All set, Bob.".into(),
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

    // No form event on a text channel.
    assert!(
        seen.iter().all(|ev| ev["type"] != "interaction_required"),
        "text channels must not receive the form event: {seen:?}"
    );

    let tool_text = tool_result_text(&seen);
    // 1. The raise degraded to the conversational directive. (The stream_chunk
    //    mirror truncates long tool results, so assert on the directive's
    //    leading content rather than trailing keys.)
    assert!(
        tool_text.contains("ask for ONE field at a time"),
        "directive returned: {tool_text}"
    );
    assert!(tool_text.contains("submit_interaction"));
    // 2. The bad email was rejected server-side (tool error the model re-asks from).
    assert!(
        tool_text.contains("must be a valid email address"),
        "bad email rejected: {tool_text}"
    );
    // 3. The good submit produced the SAME validated payload as the form path.
    assert!(tool_text.contains(r#""status":"submitted""#));
    assert!(
        tool_text.contains("+15551234567"),
        "phone normalized to E.164: {tool_text}"
    );
    assert_eq!(result.reply, "All set, Bob.");

    // 4. The identity landed on the session, same keys as the form path.
    let contact = state.session_contact(SESSION_ID);
    assert_eq!(contact.email.as_deref(), Some("bob@example.com"));
    assert_eq!(contact.phone.as_deref(), Some("+15551234567"));
}

#[tokio::test]
async fn create_session_records_the_declared_capabilities() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage, config());
    let (tx, mut rx) = unbounded_channel::<Value>();

    // With capabilities.
    handler::handle_frame(
        &state,
        &AccessContext::anonymous(),
        "conn-1",
        None,
        None,
        &handler::UserScope::Unscoped,
        &json!({
            "action": "create_conversation_session",
            "requestId": "req-create-1",
            "agentId": "11111111-1111-1111-1111-111111111111",
            "supports": ["identity_form", "some_future_capability"]
        })
        .to_string(),
        &tx,
    )
    .await;
    let (created, _) = await_event(&mut rx, "immediate_response").await;
    let session_id = created["data"]["sessionId"].as_str().expect("sessionId");
    let caps = state.session_capabilities(session_id);
    assert!(
        caps.contains("identity_form"),
        "declared capability recorded"
    );
    assert!(
        caps.contains("some_future_capability"),
        "unknown capabilities are kept (forward-compatible)"
    );

    // Without them (an SMS-style client).
    handler::handle_frame(
        &state,
        &AccessContext::anonymous(),
        "conn-2",
        None,
        None,
        &handler::UserScope::Unscoped,
        &json!({
            "action": "create_conversation_session",
            "requestId": "req-create-2",
            "agentId": "11111111-1111-1111-1111-111111111111"
        })
        .to_string(),
        &tx,
    )
    .await;
    let (created2, _) = await_event(&mut rx, "immediate_response").await;
    let session_id2 = created2["data"]["sessionId"].as_str().expect("sessionId");
    assert!(state.session_capabilities(session_id2).is_empty());
}

#[tokio::test]
async fn submit_without_a_pending_interaction_is_a_clean_error() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage, config());
    state.insert_session(test_session());
    let (tx, mut rx) = unbounded_channel::<Value>();

    submit_frame(
        &state,
        &tx,
        json!({
            "action": "submit_interaction",
            "requestId": REQUEST_ID,
            "sessionId": SESSION_ID,
            "interactionId": "int-x",
            "values": { "email": "a@b.co" }
        }),
    )
    .await;
    let (err, _) = await_event(&mut rx, "error").await;
    assert_eq!(err["error"]["code"], "NO_PENDING_INTERACTION");
}

/// th-6fbab2 — a turn cancelled while PARKED on an interaction must leave
/// nothing behind.
///
/// `cancel` and a mid-turn client disconnect both resolve to `handle.abort()`
/// in `server.rs`, which drops the turn future at its current `.await` — and a
/// parked raise tool is sitting on exactly such an `.await`. The teardown used
/// to be a statement *after* that await, so it was skipped precisely when a park
/// was outstanding: the registration outlived the turn and the visitor's
/// still-rendered card could be submitted against it indefinitely, stamping the
/// session's contact metadata each time.
///
/// Note this can only be observed here, at the runner: `cancel` is handled in
/// `server.rs` and deliberately never reaches `handle_frame`, so no test driving
/// frames alone can see it.
#[tokio::test]
async fn cancelling_a_parked_turn_clears_the_registration_and_cannot_stamp_identity() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage.clone(), config());
    state.insert_session(test_session());
    let (tx, mut rx) = unbounded_channel::<Value>();

    let turn = spawn_turn(
        state.clone(),
        storage as Arc<dyn StorageAdapter>,
        raising_mock(),
        &["identity_form"],
        tx.clone(),
    );

    let (pending, _) = await_event(&mut rx, "interaction_required").await;
    let interaction_id = pending["data"]["data"]["interactionId"]
        .as_str()
        .expect("interactionId")
        .to_string();
    assert!(
        state.pending_interaction(SESSION_ID).is_some(),
        "precondition: the turn is parked"
    );

    // What `cancel` / a mid-turn disconnect does to the turn task.
    turn.abort();
    let _ = turn.await;

    assert!(
        state.pending_interaction(SESSION_ID).is_none(),
        "a cancelled turn must not leave its park registered"
    );

    // The card is still on the visitor's screen; submitting it now must resolve
    // nothing AND have no side effect on the session.
    submit_frame(
        &state,
        &tx,
        json!({
            "action": "submit_interaction",
            "requestId": REQUEST_ID,
            "sessionId": SESSION_ID,
            "interactionId": interaction_id,
            "kind": "identity_intake",
            "values": { "email": "ghost@example.com" }
        }),
    )
    .await;
    let (err, _) = await_event(&mut rx, "error").await;
    assert_eq!(err["error"]["code"], "NO_PENDING_INTERACTION");
    assert!(
        !session_metadata(&state).contains_key("contactEmail"),
        "a submit against a cancelled turn's park must not stamp contact metadata"
    );
}

/// th-6fbab2, second half — the identity attach is a side effect on the SESSION,
/// so it must run only when the parked raise was actually resumed.
///
/// The guard above narrows the window but cannot close it: a turn can end
/// between the handler's peek (which leaves the park in place for a resubmit)
/// and the resolve. Registration present, responder dead — and the attach used
/// to run BEFORE the resolve discovered that.
#[tokio::test]
async fn a_submit_that_resolves_nothing_does_not_stamp_session_identity() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage, config());
    state.insert_session(test_session());
    let (tx, mut rx) = unbounded_channel::<Value>();

    // A park whose turn is already gone: the receiving half is dropped, so the
    // responder's `send` fails.
    let (responder, dead) = unbounded_channel();
    drop(dead);
    state.register_interaction(
        SESSION_ID,
        PendingInteraction {
            interaction_id: "int-orphan".into(),
            kind: "identity_intake".into(),
            spec: json!({ "fields": [{ "key": "email", "required": true }] }),
            responder,
        },
    );

    submit_frame(
        &state,
        &tx,
        json!({
            "action": "submit_interaction",
            "requestId": REQUEST_ID,
            "sessionId": SESSION_ID,
            "interactionId": "int-orphan",
            "kind": "identity_intake",
            "values": { "email": "ghost@example.com" }
        }),
    )
    .await;

    let (err, _) = await_event(&mut rx, "error").await;
    assert_eq!(err["error"]["code"], "NO_PENDING_INTERACTION");
    assert!(
        !session_metadata(&state).contains_key("contactEmail"),
        "the identity attach must not run for a submit that resumed nothing"
    );
}

/// The session's metadata map (empty when unset).
fn session_metadata(state: &AppState) -> std::collections::HashMap<String, Value> {
    state
        .get_session(SESSION_ID)
        .and_then(|s| s.metadata)
        .unwrap_or_default()
}

/// A reconnect is a resume: the client re-opens the socket and re-issues
/// `create_conversation_session` with the same `conversationId`, which mints a
/// NEW session id. Capabilities that lived only on the per-pod session registry
/// were therefore lost unless the client re-declared them every time — and Rich
/// Interactions then silently stopped being offered, with no error on the wire
/// (th-13df6d). They now ride durable conversation metadata, so a reconnect that
/// omits `supports` inherits what the conversation last declared.
#[tokio::test]
async fn reconnect_resuming_a_conversation_keeps_the_declared_capabilities() {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let state = AppState::new(storage, config());
    let (tx, mut rx) = unbounded_channel::<Value>();

    let create = |body: Value| {
        let state = state.clone();
        let tx = tx.clone();
        async move {
            handler::handle_frame(
                &state,
                &AccessContext::anonymous(),
                "conn",
                None,
                None,
                &handler::UserScope::Unscoped,
                &body.to_string(),
                &tx,
            )
            .await;
        }
    };

    // First connection: declares the capability.
    create(json!({
        "action": "create_conversation_session",
        "requestId": "req-conn-1",
        "agentId": "11111111-1111-1111-1111-111111111111",
        "supports": ["identity_form"]
    }))
    .await;
    let (first, _) = await_event(&mut rx, "immediate_response").await;
    let conversation_id = first["data"]["conversationId"]
        .as_str()
        .expect("conversationId")
        .to_string();
    assert!(state
        .session_capabilities(first["data"]["sessionId"].as_str().expect("sessionId"))
        .contains("identity_form"));

    // Reconnect: same conversation, `supports` omitted (the client re-declares
    // nothing — exactly what a widget resuming from its stored conversationId
    // does). Without the durable record this is where the feature went dark.
    create(json!({
        "action": "create_conversation_session",
        "requestId": "req-conn-2",
        "agentId": "11111111-1111-1111-1111-111111111111",
        "conversationId": conversation_id
    }))
    .await;
    let (resumed, _) = await_event(&mut rx, "immediate_response").await;
    assert_eq!(
        resumed["data"]["conversationId"].as_str(),
        Some(conversation_id.as_str()),
        "the reconnect resumed the same conversation"
    );
    assert!(
        state
            .session_capabilities(resumed["data"]["sessionId"].as_str().expect("sessionId"))
            .contains("identity_form"),
        "a reconnect that omits 'supports' inherits the conversation's declared capabilities"
    );

    // A resume that DECLARES wins over the inherited set, in both directions —
    // so a text-only client resuming a rich conversation opts out with `[]`
    // rather than being handed cards it cannot render.
    create(json!({
        "action": "create_conversation_session",
        "requestId": "req-conn-3",
        "agentId": "11111111-1111-1111-1111-111111111111",
        "conversationId": conversation_id,
        "supports": []
    }))
    .await;
    let (text_only, _) = await_event(&mut rx, "immediate_response").await;
    let text_only_session = text_only["data"]["sessionId"].as_str().expect("sessionId");
    assert!(
        state.session_capabilities(text_only_session).is_empty(),
        "an explicit empty 'supports' declares text-only and never inherits"
    );

    // ...and that explicit opt-out is itself durable: the NEXT reconnect that
    // omits `supports` must not resurrect the capability from a stale record.
    create(json!({
        "action": "create_conversation_session",
        "requestId": "req-conn-4",
        "agentId": "11111111-1111-1111-1111-111111111111",
        "conversationId": conversation_id
    }))
    .await;
    let (after_opt_out, _) = await_event(&mut rx, "immediate_response").await;
    assert!(
        state
            .session_capabilities(
                after_opt_out["data"]["sessionId"]
                    .as_str()
                    .expect("sessionId")
            )
            .is_empty(),
        "the text-only declaration replaced the durable record"
    );
}
