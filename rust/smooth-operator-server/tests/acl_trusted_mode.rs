//! Trusted-mode ACL enforcement (server/runner level, offline, `MockLlmClient`).
//!
//! Proves that under `AUTH_MODE=trusted` a **forwarded identity** (a
//! `base64url(JSON)` claims blob in the same `?token=` slot a JWT would ride)
//! drives the **exact same** document-level ACL enforcement a signed JWT does:
//!
//! - a forwarded identity carrying group `github:acme/secret` **CAN** read the
//!   private doc;
//! - a forwarded identity **without** that group **CANNOT**;
//! - a **malformed / absent** forwarded identity fails closed to **anonymous**
//!   (org-public only) — never admin, never the private doc.
//!
//! This is the trusted-mode analogue of `acl_chat_leak.rs`. It exercises the
//! real `TrustedIdentityVerifier` → `Principal::access_context()` → runner ACL
//! path (no network, no key — a `MockLlmClient` scripts the `knowledge_search`
//! tool call), so the security boundary is asserted end-to-end at the layer the
//! `/ws` connect path and the Lambda `send_message` path both consume.

use std::sync::Arc;

use base64::Engine as _;
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::{AccessContext, DocAcl};
use smooth_operator::adapter::StorageAdapter;
use smooth_operator::auth::{AuthVerifier, TrustedIdentityVerifier};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::{Document, DocumentType, LlmConfig};

use smooth_operator_server::runner::{self, TurnRequest, TurnResult};

/// The private-repo group ACL — exactly what `ingestion/connectors/github.rs`
/// stamps for a private repo (`github:owner/repo`).
const PRIVATE_GROUP: &str = "github:acme/secret";

/// The org the forwarded identities below claim. Knowledge is seeded *as* this
/// org because a turn's `AccessContext` carries the principal's org, and the
/// knowledge reader now enforces the tenant boundary before the ACL (feature gap
/// G7) — so an unstamped document belongs to no tenant and is invisible to one.
const ORG: &str = "acme";

fn mock_llm() -> LlmConfig {
    LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o")
}

/// Encode a claims object as the `base64url(JSON)` blob a trusted upstream would
/// forward in the `?token=` / `send_message.token` slot.
fn forward(claims: Value) -> String {
    let json = serde_json::to_vec(&claims).expect("serialize claims");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

/// Resolve the connection's `AccessContext` exactly as the `/ws` and Lambda
/// connect paths do: verify the forwarded value with the configured verifier;
/// any error (or absent input) fails closed to `anonymous()`. This is the same
/// fail-closed shape as `server::resolve_ws_access` / `dispatch::resolve_frame_access`.
fn resolve_access(verifier: &dyn AuthVerifier, forwarded: Option<&str>) -> AccessContext {
    let Some(value) = forwarded.map(str::trim).filter(|t| !t.is_empty()) else {
        return AccessContext::anonymous();
    };
    match verifier.verify(value) {
        Ok(principal) => principal.access_context(),
        Err(_) => AccessContext::anonymous(),
    }
}

/// Seed two docs sharing the term "alpha": one org-public, one ACL-restricted to
/// [`PRIVATE_GROUP`]. Mirrors `acl_chat_leak::seeded_storage`.
fn seeded_storage() -> Arc<InMemoryStorageAdapter> {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    // Ingest through the ORG-BOUND seam, exactly as `server::seed_knowledge` and
    // the admin connector-index path do: it stamps the owning tenant on each doc.
    let kb = storage.knowledge_for_access(&AccessContext::default().with_organization_id(ORG));

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
    let private = DocAcl::for_groups([PRIVATE_GROUP]).attach_to(private);
    kb.ingest(private).expect("ingest private doc");

    storage
}

/// Drive one real `run_streaming_turn` as `access`, scripting the model to issue
/// a `knowledge_search("alpha")`. Mirrors `acl_chat_leak::run_turn_as`.
async fn run_turn_as(
    storage: Arc<InMemoryStorageAdapter>,
    access: AccessContext,
) -> (TurnResult, Vec<Value>) {
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
            conversation_id: "conv-trusted-acl",
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

fn citation_sources(result: &TurnResult) -> Vec<String> {
    result
        .citations
        .iter()
        .flat_map(|c| [c.title.clone(), c.id.clone()])
        .collect()
}

/// A forwarded identity carrying the private-repo group CAN read the private doc
/// — the trusted identity drives the SAME ACL enforcement a JWT would.
#[tokio::test]
async fn trusted_identity_with_group_can_read_private_doc() {
    let verifier = TrustedIdentityVerifier::new();
    let forwarded = forward(json!({
        "sub": "acme-dev",
        "org": "acme",
        "role": "basic",
        "groups": [PRIVATE_GROUP],
    }));
    let access = resolve_access(&verifier, Some(&forwarded));

    let (result, events) = run_turn_as(seeded_storage(), access).await;
    let tool_text = tool_result_text(&events);

    assert!(
        tool_text.contains("launch codes"),
        "entitled forwarded identity MUST see the private doc; tool result:\n{tool_text}"
    );
    assert!(
        citation_sources(&result)
            .iter()
            .any(|s| s.contains("acme/secret")),
        "entitled forwarded identity should be able to cite the private doc"
    );
}

/// A forwarded identity WITHOUT the private-repo group CANNOT read the private
/// doc (but still sees org-public) — same denial a JWT-without-the-group gets.
#[tokio::test]
async fn trusted_identity_without_group_cannot_read_private_doc() {
    let verifier = TrustedIdentityVerifier::new();
    let forwarded = forward(json!({
        "sub": "random-user",
        "org": "acme",
        "role": "basic",
        "groups": ["eng"],
    }));
    let access = resolve_access(&verifier, Some(&forwarded));

    let (result, events) = run_turn_as(seeded_storage(), access).await;
    let tool_text = tool_result_text(&events);

    assert!(
        tool_text.contains("office hours"),
        "unentitled forwarded identity should still see org-public knowledge; tool result:\n{tool_text}"
    );
    assert!(
        !tool_text.contains("launch codes"),
        "LEAK: unentitled forwarded identity saw the private doc; tool result:\n{tool_text}"
    );
    assert!(
        !citation_sources(&result)
            .iter()
            .any(|s| s.contains("acme/secret")),
        "LEAK: unentitled forwarded identity cited the private doc"
    );
}

/// A MALFORMED forwarded identity fails closed to anonymous (org-public only) —
/// it does NOT become admin and does NOT see the private doc.
#[tokio::test]
async fn malformed_trusted_identity_fails_closed_to_anonymous() {
    let verifier = TrustedIdentityVerifier::new();
    // Not valid base64url → verify() errors → resolve_access → anonymous.
    let access = resolve_access(&verifier, Some("!!!not-a-valid-blob!!!"));
    assert_eq!(
        access,
        AccessContext::anonymous(),
        "malformed forwarded identity MUST resolve to anonymous, not a fabricated principal"
    );

    let (result, events) = run_turn_as(seeded_storage(), access).await;
    let tool_text = tool_result_text(&events);

    assert!(
        tool_text.contains("office hours"),
        "anonymous (malformed) should still see org-public knowledge; tool result:\n{tool_text}"
    );
    assert!(
        !tool_text.contains("launch codes"),
        "LEAK: malformed-identity connection saw the private doc; tool result:\n{tool_text}"
    );
    assert!(
        !citation_sources(&result)
            .iter()
            .any(|s| s.contains("acme/secret")),
        "LEAK: malformed-identity connection cited the private doc"
    );
}

/// An ABSENT forwarded identity (no token in the slot) likewise fails closed to
/// anonymous — never admin, never the private doc.
#[tokio::test]
async fn absent_trusted_identity_fails_closed_to_anonymous() {
    let verifier = TrustedIdentityVerifier::new();
    let access = resolve_access(&verifier, None);
    assert_eq!(access, AccessContext::anonymous());

    let (result, events) = run_turn_as(seeded_storage(), access).await;
    let tool_text = tool_result_text(&events);

    assert!(
        tool_text.contains("office hours"),
        "anonymous (absent) should still see org-public knowledge; tool result:\n{tool_text}"
    );
    assert!(
        !tool_text.contains("launch codes"),
        "LEAK: absent-identity connection saw the private doc; tool result:\n{tool_text}"
    );
    assert!(
        !citation_sources(&result)
            .iter()
            .any(|s| s.contains("acme/secret")),
        "LEAK: absent-identity connection cited the private doc"
    );
}
