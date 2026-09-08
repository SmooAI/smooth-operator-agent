//! Per-turn knowledge **org scoping** (the last multi-tenant gap), server/runner
//! level, offline (`MockLlmClient`).
//!
//! `StorageAdapter::knowledge_for_access(&self, access)` historically carried
//! only `user_id` + `groups` — NO org. A multi-tenant relational backend
//! (SmooAI) therefore could not scope RAG to the turn's tenant; it was pinned to
//! a single static org. This test proves the contributed-back fix: the turn's
//! org now rides on the [`AccessContext`], so a host adapter's
//! `knowledge_for_access` can read it.
//!
//! The harness drives one real `run_streaming_turn` against a **recording stub
//! `StorageAdapter`** that delegates to an in-memory adapter but captures the
//! `organization_id` it sees in `knowledge_for_access`. We assert the turn's org
//! reaches the adapter's knowledge seam.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::{
    ConversationUpdate, MessagePage, MessageQuery, SessionUpdate, StorageAdapter,
};
use smooth_operator::domain::{Conversation, Message, Participant, Session};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::{CheckpointStore, KnowledgeBase, LlmConfig};

use smooth_operator_server::runner::{self, TurnRequest};

/// The tenant org the turn is scoped to.
const TURN_ORG: &str = "org-tenant-acme";

/// A throwaway LLM config (never actually called — the mock provider answers).
fn mock_llm() -> LlmConfig {
    LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o")
}

/// A `StorageAdapter` that delegates every call to an inner in-memory adapter,
/// but **records the `organization_id`** it is asked to scope knowledge to in
/// [`knowledge_for_access`]. This is the seam a multi-tenant host implements:
/// it proves the per-turn org actually reaches the adapter.
struct OrgRecordingAdapter {
    inner: Arc<InMemoryStorageAdapter>,
    /// The org observed on the most recent `knowledge_for_access` call.
    seen_org: Arc<Mutex<Option<Option<String>>>>,
}

impl OrgRecordingAdapter {
    fn new() -> (Self, Arc<Mutex<Option<Option<String>>>>) {
        let seen_org = Arc::new(Mutex::new(None));
        (
            Self {
                inner: Arc::new(InMemoryStorageAdapter::new()),
                seen_org: Arc::clone(&seen_org),
            },
            seen_org,
        )
    }
}

#[async_trait]
impl StorageAdapter for OrgRecordingAdapter {
    async fn create_conversation(
        &self,
        conversation: Conversation,
    ) -> anyhow::Result<Conversation> {
        self.inner.create_conversation(conversation).await
    }
    async fn get_conversation(&self, id: &str) -> anyhow::Result<Option<Conversation>> {
        self.inner.get_conversation(id).await
    }
    async fn list_conversations_by_org(
        &self,
        organization_id: &str,
    ) -> anyhow::Result<Vec<Conversation>> {
        self.inner.list_conversations_by_org(organization_id).await
    }
    async fn update_conversation(
        &self,
        id: &str,
        update: ConversationUpdate,
    ) -> anyhow::Result<Conversation> {
        self.inner.update_conversation(id, update).await
    }
    async fn add_participant(&self, participant: Participant) -> anyhow::Result<Participant> {
        self.inner.add_participant(participant).await
    }
    async fn get_participant(&self, id: &str) -> anyhow::Result<Option<Participant>> {
        self.inner.get_participant(id).await
    }
    async fn list_participants_by_conversation(
        &self,
        conversation_id: &str,
    ) -> anyhow::Result<Vec<Participant>> {
        self.inner
            .list_participants_by_conversation(conversation_id)
            .await
    }
    async fn resolve_participant_by_external_id(
        &self,
        conversation_id: &str,
        external_id: &str,
    ) -> anyhow::Result<Option<Participant>> {
        self.inner
            .resolve_participant_by_external_id(conversation_id, external_id)
            .await
    }
    async fn append_message(&self, message: Message) -> anyhow::Result<Message> {
        self.inner.append_message(message).await
    }
    async fn get_message(&self, id: &str) -> anyhow::Result<Option<Message>> {
        self.inner.get_message(id).await
    }
    async fn list_messages_by_conversation(
        &self,
        query: MessageQuery,
    ) -> anyhow::Result<MessagePage> {
        self.inner.list_messages_by_conversation(query).await
    }
    async fn create_session(&self, session: Session) -> anyhow::Result<Session> {
        self.inner.create_session(session).await
    }
    async fn get_session(&self, session_id: &str) -> anyhow::Result<Option<Session>> {
        self.inner.get_session(session_id).await
    }
    async fn update_session(
        &self,
        session_id: &str,
        update: SessionUpdate,
    ) -> anyhow::Result<Session> {
        self.inner.update_session(session_id, update).await
    }
    async fn list_sessions_by_conversation(
        &self,
        conversation_id: &str,
    ) -> anyhow::Result<Vec<Session>> {
        self.inner
            .list_sessions_by_conversation(conversation_id)
            .await
    }
    fn checkpoints(&self) -> Arc<dyn CheckpointStore> {
        self.inner.checkpoints()
    }
    fn knowledge(&self) -> Arc<dyn KnowledgeBase> {
        self.inner.knowledge()
    }
    fn knowledge_for_access(&self, access: &AccessContext) -> Arc<dyn KnowledgeBase> {
        // THE assertion seam: record the org the turn scoped retrieval to. A real
        // multi-tenant host would use it to pick the tenant's documents here.
        *self.seen_org.lock().unwrap() = Some(access.organization_id.clone());
        self.inner.knowledge_for_access(access)
    }
}

/// Drain all queued protocol events from the runner's sink.
async fn drain(mut rx: UnboundedReceiver<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    while let Some(ev) = rx.recv().await {
        out.push(ev);
    }
    out
}

/// Drive one real `run_streaming_turn` with `access`, scripting the model to
/// issue a `knowledge_search` (so the ACL-guarded retrieval seam — and thus
/// `knowledge_for_access` — is exercised) and then answer.
async fn run_turn_as(storage: Arc<dyn StorageAdapter>, access: AccessContext) {
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

    let (tx, rx): (_, UnboundedReceiver<Value>) = unbounded_channel();

    runner::run_streaming_turn(
        TurnRequest {
            demo_tools: false,
            storage,
            llm: mock_llm(),
            max_iterations: 4,
            conversation_id: "conv-org-scope",
            request_id: "req-1",
            user_message: "Tell me about alpha",
            model_max_output: None,
            access,
            llm_provider: Some(Arc::new(mock.clone())),
            executor: None,
            reranker: None,
            confirmation: None,
            interactions: None,
            tool_provider: None,
            tool_hooks: vec![],
            system_prompt: None,
            org_id: Some(TURN_ORG.to_string()),
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
    let _ = drain(rx).await;
}

/// A turn whose `AccessContext` carries the conversation's org makes that org
/// visible to the storage adapter's `knowledge_for_access` — the seam a
/// multi-tenant host scopes RAG with.
#[tokio::test]
async fn turn_org_reaches_knowledge_for_access() {
    let (adapter, seen_org) = OrgRecordingAdapter::new();
    let storage: Arc<dyn StorageAdapter> = Arc::new(adapter);

    // The requester's entitlement carries the turn's org (as the handler/lambda
    // now populate it from the session/conversation).
    let access = AccessContext::new(Some("user-1".into()), vec!["eng".into()])
        .with_organization_id(TURN_ORG);

    run_turn_as(storage, access).await;

    let observed = seen_org
        .lock()
        .unwrap()
        .clone()
        .expect("knowledge_for_access must have been called during the turn");
    assert_eq!(
        observed,
        Some(TURN_ORG.to_string()),
        "the turn's org must reach the storage adapter's knowledge_for_access seam"
    );
}

/// Behavior-preserving: a context with no org (the single-tenant / anonymous
/// default) reaches `knowledge_for_access` with `organization_id == None`, so an
/// adapter that ignores org is unaffected.
#[tokio::test]
async fn no_org_context_is_none_at_knowledge_seam() {
    let (adapter, seen_org) = OrgRecordingAdapter::new();
    let storage: Arc<dyn StorageAdapter> = Arc::new(adapter);

    run_turn_as(storage, AccessContext::anonymous()).await;

    let observed = seen_org
        .lock()
        .unwrap()
        .clone()
        .expect("knowledge_for_access must have been called during the turn");
    assert_eq!(
        observed, None,
        "an org-less context must reach the knowledge seam as None (single-tenant default)"
    );
}
