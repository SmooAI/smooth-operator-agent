//! Regression: the `eventual_response` reply must fall back to THIS turn's
//! accumulated streamed answer when the engine's terminal assistant entry has
//! empty content (a tool-call or reasoning-only final message).
//!
//! Reproduces the prod symptom on reasoning models (e.g. groq-gpt-oss-120b):
//! the answer streams token-by-token, but the loop's LAST assistant message is
//! reasoning-only, so `Conversation::last_assistant_content()` returns "" — and
//! the old code shipped an EMPTY `eventual_response` (blank `responseParts`,
//! dropped `suggestedNextActions`) even though the full reply streamed. The fix
//! prefers the streamed text of the turn as the authoritative final reply.
//! (th-emptyreply)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc::unbounded_channel;

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator::tool_provider::{ToolProvider, ToolProviderContext};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::{LlmConfig, Tool, ToolSchema};

use smooth_operator_server::runner::{self, TurnRequest};

const CONVERSATION_ID: &str = "conv-emptyreply-1";
const REQUEST_ID: &str = "req-emptyreply-1";
const TOOL: &str = "lookup";
/// The real answer, carrying a suggested-replies trailer, streamed in the FIRST
/// iteration — the same iteration that also emits a tool call.
const STREAMED_ANSWER: &str =
    "Here is the full answer to your question.\n<suggested_replies>[\"Tell me more\", \"That's all\"]</suggested_replies>";

/// A trivial tool that just records it ran; its only job is to force a second
/// agent-loop iteration after the answer already streamed.
struct RecordingTool {
    executed: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for RecordingTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: TOOL.into(),
            description: "records execution".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn execute(&self, _arguments: Value) -> anyhow::Result<String> {
        self.executed.store(true, Ordering::SeqCst);
        Ok("looked up".into())
    }
}

struct RecordingProvider {
    executed: Arc<AtomicBool>,
}

#[async_trait]
impl ToolProvider for RecordingProvider {
    async fn tools_for(&self, _ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(RecordingTool {
            executed: self.executed.clone(),
        })]
    }
}

/// Scripted turn:
///   - iteration 1 streams the real answer (Delta) AND a tool call, so the loop
///     pushes an assistant message and runs a second iteration;
///   - iteration 2 is reasoning-only (empty Delta content, no tool calls), so
///     the engine's terminal assistant entry has EMPTY content and
///     `last_assistant_content()` returns "".
fn scripted_mock() -> MockLlmClient {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::Delta {
            content: STREAMED_ANSWER.into(),
        },
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: TOOL.into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk: "{}".into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ])
    .push_stream(vec![
        StreamEvent::Reasoning {
            content: "I've already answered; nothing more to add.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);
    mock
}

#[tokio::test]
async fn empty_terminal_content_falls_back_to_streamed_reply() {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());
    let executed = Arc::new(AtomicBool::new(false));
    let provider = Arc::new(RecordingProvider {
        executed: executed.clone(),
    });
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
            llm_provider: Some(Arc::new(scripted_mock())),
            executor: None,
            reranker: None,
            confirmation: None,
            interactions: None,
            tool_provider: Some(provider),
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

    // Sanity: the second iteration actually ran (the tool was invoked).
    assert!(executed.load(Ordering::SeqCst), "tool must have executed");

    // The eventual_response reply is the streamed answer with the trailer
    // stripped — NOT the empty terminal assistant content.
    assert_eq!(result.reply, "Here is the full answer to your question.");
    // Suggestions parsed off the streamed trailer survive too.
    assert_eq!(
        result.suggested_next_actions,
        vec!["Tell me more", "That's all"]
    );

    // And the answer really did stream to the client (the marker never leaks).
    let mut streamed = String::new();
    while let Ok(ev) = rx.try_recv() {
        if ev["type"] == "stream_token" {
            for path in [
                &ev["data"]["data"]["token"],
                &ev["data"]["token"],
                &ev["token"],
            ] {
                if let Some(tok) = path.as_str() {
                    streamed.push_str(tok);
                    break;
                }
            }
        }
    }
    assert!(
        streamed.contains("Here is the full answer"),
        "answer should have streamed to the client: {streamed:?}"
    );
    assert!(
        !streamed.contains("<suggested_replies>"),
        "trailer marker must not leak into the stream: {streamed:?}"
    );
}
