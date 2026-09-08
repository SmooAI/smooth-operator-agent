//! THE headline leak test (server/runner level, offline, `MockLlmClient`).
//!
//! This is the #1 adversarial-review security finding: the document-level ACL
//! layer was dead on the **live chat path**, so a private GitHub repo was
//! retrievable by ANY chat user. The runner (`run_streaming_turn`) queried
//! `storage.knowledge()` **raw** — no `AccessContext`, no ACL reader — for both
//! the auto-injected `[Relevant knowledge]` context and the `knowledge_search`
//! tool.
//!
//! ## TDD — this test was written FIRST and FAILED before the fix
//!
//! Before the runner threaded an `AccessContext` and read through
//! `storage.knowledge_for_access(&access)`, a user with NO entitlement to a
//! private-repo doc still saw it in the tool result the model reads — the
//! `private_doc_not_leaked_to_unentitled_user` assertion below failed (the
//! private "launch codes" content reached the model). The fix drops it.
//!
//! Runs fully offline: a `MockLlmClient` scripts the `knowledge_search` call so
//! there is no network / API key. We assert on the tool result the model reads
//! AND the turn's citations — a restricted doc must appear in neither.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::{AccessContext, DocAcl};
use smooth_operator::adapter::StorageAdapter;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::{Document, DocumentType, LlmConfig};

use smooth_operator_server::runner::{self, TurnRequest, TurnResult};

/// The private-repo group ACL — exactly what `ingestion/connectors/github.rs`
/// stamps for a private repo (`github:owner/repo`).
const PRIVATE_GROUP: &str = "github:acme/secret";

/// A throwaway LLM config (never actually called — the mock provider answers).
fn mock_llm() -> LlmConfig {
    LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o")
}

/// Seed a fresh in-memory storage adapter with exactly two docs sharing the
/// query term `"alpha"`:
/// - `doc-public` — org-public (no ACL): everyone may read it.
/// - `doc-private` — ACL-restricted to group [`PRIVATE_GROUP`] (a private-repo
///   doc): only a requester carrying that group may read it.
///
/// Both are ingested through `storage.knowledge()` (the ACL-recording ingest
/// handle), so the adapter's ACL side table is populated — exactly the
/// ingest→serve flow the production seeding / `/index` path uses.
fn seeded_storage() -> Arc<InMemoryStorageAdapter> {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    let kb = storage.knowledge();

    let mut public = Document::new(
        "The alpha office hours are open to the whole organization.",
        "handbook/hours.md",
        DocumentType::Documentation,
    );
    public.id = "doc-public".to_string();
    kb.ingest(public).expect("ingest public doc");

    let mut private = Document::new(
        "The alpha launch codes live in the private acme/secret repository.",
        "acme/secret/CODES.md",
        DocumentType::Documentation,
    );
    private.id = "doc-private".to_string();
    // Stamp the private-repo group ACL (same shape ingestion stamps).
    let private = DocAcl::for_groups([PRIVATE_GROUP]).attach_to(private);
    kb.ingest(private).expect("ingest private doc");

    storage
}

/// Drive one real `run_streaming_turn` as `access`, scripting the model to issue
/// a `knowledge_search` for the shared term "alpha". Returns the turn result and
/// the drained protocol events.
async fn run_turn_as(
    storage: Arc<InMemoryStorageAdapter>,
    access: AccessContext,
) -> (TurnResult, Vec<Value>) {
    // Script the STREAMING path (the runner drives `run_with_channel`, which
    // calls `chat_stream`): turn 1 streams a `knowledge_search("alpha")` tool
    // call; turn 2 streams the final answer. This forces the `knowledge_search`
    // tool path (the second ACL-guarded retrieval surface) on top of the
    // auto-injected context.
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
            content: "Here is what I found about alpha.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);

    let (tx, rx): (_, UnboundedReceiver<Value>) = unbounded_channel();
    let storage: Arc<dyn StorageAdapter> = storage;

    let result = runner::run_streaming_turn(
        TurnRequest {
            demo_tools: false,
            storage,
            llm: mock_llm(),
            max_iterations: 4,
            conversation_id: "conv-acl-leak",
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
    let events = drain(rx).await;
    (result, events)
}

/// Drain all queued protocol events from the runner's sink.
async fn drain(mut rx: UnboundedReceiver<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    // Anything still in flight (the channel was closed by `drop(tx)`).
    while let Some(ev) = rx.recv().await {
        out.push(ev);
    }
    out
}

/// Concatenate every `knowledge_search` tool-result string emitted as a
/// `stream_chunk` — this is exactly the content the model reads.
fn tool_result_text(events: &[Value]) -> String {
    let mut s = String::new();
    for ev in events {
        // stream_chunk events nest the tool result under
        // `data.state.rawResponse.toolResult.result` (see protocol::stream_chunk).
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

/// Every citation source string on the turn result (the `title` carries the
/// document's source path; `id` is the document id).
fn citation_sources(result: &TurnResult) -> Vec<String> {
    result
        .citations
        .iter()
        .flat_map(|c| [c.title.clone(), c.id.clone()])
        .collect()
}

/// THE LEAK TEST: a user **without** the private-repo group must NEVER see the
/// private doc — not in the tool result the model reads, not in any citation.
/// The public doc still comes through.
#[tokio::test]
async fn private_doc_not_leaked_to_unentitled_user() {
    let storage = seeded_storage();

    // A normal chat user: authenticated but NOT a member of github:acme/secret.
    let outsider = AccessContext::new(Some("random-user".to_string()), vec!["eng".to_string()]);
    let (result, events) = run_turn_as(storage, outsider).await;

    let tool_text = tool_result_text(&events);
    let sources = citation_sources(&result);

    // The public doc is visible.
    assert!(
        tool_text.contains("office hours"),
        "public alpha doc should be visible to any user; tool result:\n{tool_text}"
    );

    // The private-repo doc must NOT appear in what the model reads...
    assert!(
        !tool_text.contains("launch codes"),
        "LEAK: private-repo content reached the model as an unentitled user; tool result:\n{tool_text}"
    );
    // ...nor its source path...
    assert!(
        !tool_text.contains("acme/secret/CODES.md"),
        "LEAK: private-repo source reached the model; tool result:\n{tool_text}"
    );
    // ...nor in any citation.
    assert!(
        !sources.iter().any(|s| s.contains("acme/secret")),
        "LEAK: private-repo doc was cited to an unentitled user; citations: {sources:?}"
    );
}

/// An **anonymous** connection (no token) must also be denied the private doc —
/// fail closed for ACL'd content (it sees only org-public knowledge).
#[tokio::test]
async fn anonymous_user_sees_only_org_public() {
    let storage = seeded_storage();
    let (result, events) = run_turn_as(storage, AccessContext::anonymous()).await;

    let tool_text = tool_result_text(&events);
    assert!(
        tool_text.contains("office hours"),
        "anonymous should still see org-public knowledge; tool result:\n{tool_text}"
    );
    assert!(
        !tool_text.contains("launch codes"),
        "LEAK: anonymous saw the private-repo doc; tool result:\n{tool_text}"
    );
    assert!(
        !citation_sources(&result)
            .iter()
            .any(|s| s.contains("acme/secret")),
        "LEAK: anonymous cited the private-repo doc"
    );
}

/// The positive case: a user **with** the private-repo group DOES retrieve the
/// private doc — entitlement is honored, not just universally denied.
#[tokio::test]
async fn entitled_user_can_read_private_doc() {
    let storage = seeded_storage();

    // A member of the private repo's group.
    let insider = AccessContext::new(
        Some("acme-dev".to_string()),
        vec![PRIVATE_GROUP.to_string()],
    );
    let (result, events) = run_turn_as(storage, insider).await;

    let tool_text = tool_result_text(&events);
    assert!(
        tool_text.contains("launch codes"),
        "entitled user MUST see the private-repo doc they have access to; tool result:\n{tool_text}"
    );
    // And the public doc too.
    assert!(
        tool_text.contains("office hours"),
        "entitled user should also see org-public knowledge; tool result:\n{tool_text}"
    );
    // It should be citable for them.
    assert!(
        citation_sources(&result)
            .iter()
            .any(|s| s.contains("acme/secret")),
        "entitled user should be able to cite the private-repo doc"
    );
}
