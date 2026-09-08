//! Regression (th-1fca98): a user turn's attached images must be PERSISTED onto
//! the stored inbound message, so a DIFFERENT client reading the conversation's
//! history re-renders them (cross-client parity). Before the fix, the inbound
//! message was stored text-only (`MessageContent::from_text`), the images rode
//! the live turn only, and any other client saw text with no picture.
//!
//! Drives the real `runner::run_streaming_turn` against in-memory storage with a
//! scripted mock LLM, then reads the persisted message log back.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc::unbounded_channel;

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::{MessageQuery, StorageAdapter};
use smooth_operator::domain::Direction;
use smooth_operator::tool_provider::UserImage;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::LlmConfig;

use smooth_operator_server::runner::{self, TurnRequest};

const CONVERSATION_ID: &str = "conv-images-1";
const REQUEST_ID: &str = "req-images-1";
const IMG: &str = "data:image/png;base64,AAAABBBB";

/// One iteration: stream a short reply, then stop. No tools needed.
fn reply_mock() -> MockLlmClient {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::Delta {
            content: "nice photo".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);
    mock
}

#[tokio::test]
async fn inbound_turn_images_are_persisted_for_cross_client_render() {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());
    let (tx, _rx) = unbounded_channel::<Value>();

    runner::run_streaming_turn(
        TurnRequest {
            demo_tools: false,
            storage: storage.clone(),
            llm: LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o"),
            max_iterations: 4,
            conversation_id: CONVERSATION_ID,
            request_id: REQUEST_ID,
            user_message: "look at this",
            model_max_output: None,
            access: AccessContext::anonymous(),
            llm_provider: Some(Arc::new(reply_mock())),
            executor: None,
            reranker: None,
            confirmation: None,
            interactions: None,
            tool_provider: None,
            tool_hooks: vec![],
            system_prompt: None,
            org_id: None,
            gateway_key: None,
            workflow: None,
            judge: None,
            greeting_section: None,
            skill_section: None,
            enabled_tools: None,
            auth_gate: None,
            tool_configs: None,
            extensions: None,
            images: vec![UserImage {
                url: IMG.into(),
                detail: None,
            }],
            files: vec![],
            request_metadata: None,
        },
        &tx,
    )
    .await
    .expect("run_streaming_turn");

    // Read the persisted log the way any OTHER client would (get history).
    let page = storage
        .list_messages_by_conversation(MessageQuery::new(CONVERSATION_ID, 50))
        .await
        .expect("list messages");
    let inbound = page
        .messages
        .iter()
        .find(|m| m.direction == Direction::Inbound)
        .expect("an inbound user message was persisted");

    // The text is still there (flat mirror + text item).
    assert_eq!(inbound.content.text.as_deref(), Some("look at this"));
    // And the image now rides the stored message as an `image` content item.
    let stored_image = inbound
        .content
        .items
        .iter()
        .find(|it| it.item_type == "image")
        .expect("the attached image must be persisted onto the inbound message");
    assert_eq!(stored_image.url.as_deref(), Some(IMG));
}
