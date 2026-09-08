//! Suggested quick replies — turn-level plumbing.
//!
//! Drives [`runner::run_streaming_turn`] offline (mock LLM) to prove:
//!   - the system prompt teaches the `<suggested_replies>` trailer contract,
//!   - a trailer on the model's reply is stripped from the final reply and
//!     parsed onto [`TurnResult::suggested_next_actions`],
//!   - the raw marker never reaches the client's live token stream,
//!   - a trailer-less reply behaves exactly as before (empty suggestions),
//!   - `general_agent_response` carries the suggestions as `suggestedNextActions`.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc::unbounded_channel;

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::conversation::Role;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::LlmConfig;

use smooth_operator_server::runner::{self, TurnRequest, TurnResult};

const CONVERSATION_ID: &str = "conv-sug-1";
const REQUEST_ID: &str = "req-sug-1";

/// Run one mock turn streaming `deltas`, returning the result, the streamed
/// `stream_token` texts, and the system prompt the model saw.
async fn run_turn(deltas: &[&str]) -> (TurnResult, Vec<String>, String) {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());
    let mock = MockLlmClient::new();
    let mut events: Vec<StreamEvent> = deltas
        .iter()
        .map(|d| StreamEvent::Delta {
            content: (*d).into(),
        })
        .collect();
    events.push(StreamEvent::Done {
        finish_reason: "stop".into(),
    });
    mock.push_stream(events);
    let mock = Arc::new(mock);

    let (tx, mut rx) = unbounded_channel::<Value>();
    let result = runner::run_streaming_turn(
        TurnRequest {
            demo_tools: false,
            storage,
            llm: LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o"),
            max_iterations: 4,
            conversation_id: CONVERSATION_ID,
            request_id: REQUEST_ID,
            user_message: "How mature are our processes?",
            model_max_output: None,
            access: AccessContext::anonymous(),
            llm_provider: Some(mock.clone()),
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

    let mut streamed = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if ev["type"] == "stream_token" {
            if let Some(tok) = ev["data"]["data"]["token"].as_str() {
                streamed.push(tok.to_string());
            } else if let Some(tok) = ev["data"]["token"].as_str() {
                streamed.push(tok.to_string());
            } else if let Some(tok) = ev["token"].as_str() {
                streamed.push(tok.to_string());
            }
        }
    }

    let calls = mock.calls();
    let system = calls
        .first()
        .and_then(|c| c.messages.iter().find(|m| m.role == Role::System))
        .map(|m| m.content.clone())
        .unwrap_or_default();

    (result, streamed, system)
}

#[tokio::test]
async fn trailer_is_parsed_stripped_and_never_streamed() {
    let (result, streamed, system) = run_turn(&[
        "Pick the closest fit!",
        "\n<suggested_repl",
        "ies>[\"Ad-hoc\", \"Repeatable\", \"Optimized\"]</suggested_replies>",
    ])
    .await;

    // The prompt taught the contract.
    assert!(
        system.contains("<suggested_replies>"),
        "suggestions prompt section missing: {system}"
    );
    // Final reply is clean; suggestions parsed.
    assert_eq!(result.reply, "Pick the closest fit!");
    assert_eq!(
        result.suggested_next_actions,
        vec!["Ad-hoc", "Repeatable", "Optimized"]
    );
    // The raw marker never reached the live stream.
    let full_stream = streamed.concat();
    assert!(
        !full_stream.contains("<suggested_replies>"),
        "marker leaked into the stream: {full_stream}"
    );
    assert_eq!(full_stream, "Pick the closest fit!\n");
}

#[tokio::test]
async fn plain_reply_streams_unchanged_with_empty_suggestions() {
    let (result, streamed, _) = run_turn(&["Thanks — tell me ", "more about your team."]).await;
    assert_eq!(result.reply, "Thanks — tell me more about your team.");
    assert!(result.suggested_next_actions.is_empty());
    assert_eq!(streamed.concat(), "Thanks — tell me more about your team.");
}

#[test]
fn general_agent_response_carries_suggestions() {
    let response = runner::general_agent_response(
        "Which one fits?",
        &["Yes".to_string(), "Not yet".to_string()],
    );
    assert_eq!(
        response["suggestedNextActions"],
        serde_json::json!(["Yes", "Not yet"])
    );
    assert_eq!(
        response["responseParts"],
        serde_json::json!(["Which one fits?"])
    );
}
