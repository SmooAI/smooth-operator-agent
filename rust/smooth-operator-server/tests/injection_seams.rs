//! Host injection-seam tests (runner level, offline, `MockLlmClient`).
//!
//! Two seams let a host run the operator's chat turn with its OWN tool catalog
//! and its OWN per-org persona WITHOUT forking the runner:
//!
//! * **SEAM 1 — custom tools.** A [`ToolProvider`] contributes EXTRA tools that
//!   the runner merges into the turn's `ToolRegistry` alongside the built-ins.
//! * **SEAM 2 — per-org persona.** A resolved `system_prompt` overrides the
//!   built-in const prompt for the turn.
//!
//! Both must be behavior-preserving by default: no provider ⇒ built-ins only;
//! no persona ⇒ the existing const prompt. We assert that by inspecting the
//! [`MockLlmClient`]'s recorded calls — the tool schemas offered to the model
//! (SEAM 1) and the system message the model received (SEAM 2).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator::tool_provider::{ToolProvider, ToolProviderContext};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::{
    InMemoryMemory, LlmConfig, Memory, MemoryEntry, MemoryType, Role, Tool, ToolSchema,
};

use smooth_operator_server::runner::{self, TurnRequest, TurnResult};

/// The built-in system prompt the runner falls back to with no persona. Kept in
/// sync with `runner.rs`'s `KNOWLEDGE_CHAT_SYSTEM_PROMPT` (its opening clause is
/// stable enough to assert on without coupling to the full text).
const CONST_PROMPT_OPENING: &str = "You are a helpful customer-support agent";

/// A throwaway LLM config (never actually called — the mock answers).
fn mock_llm() -> LlmConfig {
    LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o")
}

/// A trivial host tool, used to prove an injected tool's schema reaches the LLM.
struct StubHostTool;

#[async_trait]
impl Tool for StubHostTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "host_crm_lookup".into(),
            description: "Look up a customer in the host CRM.".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(&self, _arguments: Value) -> anyhow::Result<String> {
        Ok("ok".into())
    }
}

/// The per-turn facts a provider observed, captured for assertions.
#[derive(Default)]
struct SeenCtx {
    org_id: Option<String>,
    conversation_id: Option<String>,
    gateway_key: Option<String>,
}

/// A provider that contributes [`StubHostTool`] and records the per-turn context
/// it saw (org + conversation + gateway key).
struct StubProvider {
    seen: Arc<std::sync::Mutex<SeenCtx>>,
}

#[async_trait]
impl ToolProvider for StubProvider {
    async fn tools_for(&self, ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
        let mut seen = self.seen.lock().unwrap();
        seen.org_id = ctx.org_id.clone();
        seen.conversation_id = ctx.conversation_id.clone();
        seen.gateway_key = ctx.gateway_key.clone();
        vec![Arc::new(StubHostTool) as Arc<dyn Tool>]
    }
}

/// Drive one real `run_streaming_turn` with the given seam inputs and return the
/// result plus the mock so the test can assert on what the model received. The
/// model is scripted to answer immediately (no tool call) — the assertions are
/// about the REQUEST the runner built (offered tools + system prompt), not the
/// model's behavior.
async fn run_turn(
    tool_provider: Option<Arc<dyn ToolProvider>>,
    system_prompt: Option<String>,
    org_id: Option<String>,
) -> (TurnResult, MockLlmClient) {
    run_turn_with_key(tool_provider, system_prompt, org_id, None).await
}

/// Like [`run_turn`] but also threads a resolved per-turn gateway key, so a test
/// can assert the provider sees it.
async fn run_turn_with_key(
    tool_provider: Option<Arc<dyn ToolProvider>>,
    system_prompt: Option<String>,
    org_id: Option<String>,
    gateway_key: Option<String>,
) -> (TurnResult, MockLlmClient) {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());

    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::Delta {
            content: "Done.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);

    let (tx, rx): (_, UnboundedReceiver<Value>) = unbounded_channel();
    let result = runner::run_streaming_turn(
        TurnRequest {
            demo_tools: false,
            storage,
            llm: mock_llm(),
            max_iterations: 4,
            conversation_id: "conv-seam",
            request_id: "req-1",
            user_message: "hello",
            model_max_output: None,
            access: AccessContext::anonymous(),
            llm_provider: Some(Arc::new(mock.clone())),
            executor: None,
            reranker: None,
            confirmation: None,
            interactions: None,
            tool_provider,
            tool_hooks: Vec::new(),
            system_prompt,
            org_id,
            gateway_key,
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
    // Drain the protocol events so the channel closes cleanly.
    let mut rx = rx;
    while rx.recv().await.is_some() {}

    (result, mock)
}

/// The system-prompt text the model saw on the first call (the `System` message).
fn system_prompt_seen(mock: &MockLlmClient) -> String {
    let calls = mock.calls();
    let first = calls.first().expect("at least one LLM call");
    first
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .map(|m| m.content.clone())
        .expect("a system message was sent")
}

/// The tool names offered to the model on the first call.
fn tool_names_seen(mock: &MockLlmClient) -> Vec<String> {
    let calls = mock.calls();
    let first = calls.first().expect("at least one LLM call");
    first.tools.iter().map(|t| t.name.clone()).collect()
}

/// The concatenated content of every message the model saw on the first call —
/// used to assert whether the engine auto-injected recalled memories.
fn all_message_content_seen(mock: &MockLlmClient) -> String {
    let calls = mock.calls();
    let first = calls.first().expect("at least one LLM call");
    first
        .messages
        .iter()
        .map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Drive one turn against an explicit storage adapter and user message (the
/// memory seam needs a message that lexically overlaps the stored memory, which
/// the fixed-input [`run_turn`] can't provide).
async fn run_turn_with_storage(
    storage: Arc<dyn StorageAdapter>,
    user_message: &str,
) -> MockLlmClient {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::Delta {
            content: "Done.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);

    let (tx, mut rx): (_, UnboundedReceiver<Value>) = unbounded_channel();
    runner::run_streaming_turn(
        TurnRequest {
            demo_tools: false,
            storage,
            llm: mock_llm(),
            max_iterations: 4,
            conversation_id: "conv-mem",
            request_id: "req-mem",
            user_message,
            model_max_output: None,
            access: AccessContext::anonymous(),
            llm_provider: Some(Arc::new(mock.clone())),
            executor: None,
            reranker: None,
            confirmation: None,
            interactions: None,
            tool_provider: None,
            tool_hooks: Vec::new(),
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
    while rx.recv().await.is_some() {}
    mock
}

// ---------------------------------------------------------------------------
// SEAM 3 — durable memory auto-recall (StorageAdapter::memory_for_access)
// ---------------------------------------------------------------------------

/// Default: an adapter with no memory ⇒ no auto-recall, so the model sees no
/// `[Recalled memories]` block. Guards against the seam injecting when absent.
#[tokio::test]
async fn no_memory_means_no_recall_injection() {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());
    let mock = run_turn_with_storage(storage, "add shows to my watchlist").await;
    assert!(
        !all_message_content_seen(&mock).contains("[Recalled memories]"),
        "with no memory attached the turn must carry no auto-recall block"
    );
}

/// When the adapter exposes a memory via `memory_for_access`, the engine
/// auto-recalls entries relevant to the user message and injects them into the
/// turn — the seam that lights up Big Smooth's durable auto-recall (th-374b27).
#[tokio::test]
async fn attached_memory_is_auto_recalled_into_the_turn() {
    let memory = Arc::new(InMemoryMemory::new());
    memory
        .store(MemoryEntry::new(
            "always add shows to the smoo-hub watchlist",
            MemoryType::Project,
        ))
        .expect("store memory");

    let storage: Arc<dyn StorageAdapter> =
        Arc::new(InMemoryStorageAdapter::new().with_memory(memory as Arc<dyn Memory>));

    // The message shares "add", "shows", "watchlist" with the stored memory, so
    // the engine's word-overlap recall surfaces it.
    let mock = run_turn_with_storage(storage, "add shows to my watchlist").await;

    let seen = all_message_content_seen(&mock);
    assert!(
        seen.contains("[Recalled memories]"),
        "an attached memory must inject a recall block, got: {seen}"
    );
    assert!(
        seen.contains("always add shows to the smoo-hub watchlist"),
        "the recalled memory's content must reach the model, got: {seen}"
    );
}

// ---------------------------------------------------------------------------
// SEAM 1 — custom tool injection
// ---------------------------------------------------------------------------

/// Default: no tool provider ⇒ the registry is exactly the built-ins. The only
/// tool the model is offered is `knowledge_search`; no host tool appears.
#[tokio::test]
async fn no_tool_provider_offers_only_builtins() {
    let (_r, mock) = run_turn(None, None, None).await;
    let names = tool_names_seen(&mock);
    assert_eq!(
        names,
        vec!["knowledge_search".to_string()],
        "with no provider the registry must be exactly today's built-ins"
    );
}

/// Injected provider ⇒ its tool is merged into the registry alongside the
/// built-in, so the model is offered BOTH. The provider also sees the turn's
/// org_id so a host can return per-org tools.
#[tokio::test]
async fn injected_provider_tools_reach_the_model() {
    let seen = Arc::new(std::sync::Mutex::new(SeenCtx::default()));
    let provider = Arc::new(StubProvider { seen: seen.clone() });

    let (_r, mock) = run_turn(Some(provider), None, Some("org-acme".into())).await;

    let mut names = tool_names_seen(&mock);
    names.sort();
    assert_eq!(
        names,
        vec![
            "host_crm_lookup".to_string(),
            "knowledge_search".to_string()
        ],
        "the injected host tool must be merged with the built-ins"
    );
    assert_eq!(
        seen.lock().unwrap().org_id.as_deref(),
        Some("org-acme"),
        "the provider must receive the turn's org_id for per-org tools"
    );
}

/// The provider must see the turn's `conversation_id` and resolved `gateway_key`
/// so a host's conversation-persisting tools and retrieval tools work (instead of
/// degrading to no-ops on an empty conversation id / missing key).
#[tokio::test]
async fn provider_sees_conversation_id_and_gateway_key() {
    let seen = Arc::new(std::sync::Mutex::new(SeenCtx::default()));
    let provider = Arc::new(StubProvider { seen: seen.clone() });

    let (_r, _mock) = run_turn_with_key(
        Some(provider),
        None,
        Some("org-acme".into()),
        Some("sk-org-acme".into()),
    )
    .await;

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.conversation_id.as_deref(),
        Some("conv-seam"),
        "the provider must receive the turn's conversation_id"
    );
    assert_eq!(
        seen.gateway_key.as_deref(),
        Some("sk-org-acme"),
        "the provider must receive the resolved per-org gateway key"
    );
}

// ---------------------------------------------------------------------------
// SEAM 2 — per-org agent persona
// ---------------------------------------------------------------------------

/// Default: no persona ⇒ the model receives the built-in const prompt.
#[tokio::test]
async fn no_persona_uses_const_prompt() {
    let (_r, mock) = run_turn(None, None, None).await;
    let prompt = system_prompt_seen(&mock);
    assert!(
        prompt.starts_with(CONST_PROMPT_OPENING),
        "with no persona the runner must use the const prompt, got: {prompt}"
    );
}

/// A resolved persona ⇒ it REPLACES the const prompt as the turn's system prompt.
#[tokio::test]
async fn persona_overrides_const_prompt() {
    let persona = "You are Acme's brisk, no-nonsense concierge. Be terse.";
    let (_r, mock) = run_turn(None, Some(persona.to_string()), None).await;
    let prompt = system_prompt_seen(&mock);
    assert!(
        prompt.starts_with(persona),
        "a resolved persona must lead the system prompt verbatim, got: {prompt}"
    );
    assert!(
        prompt.contains("<suggested_replies>"),
        "the suggested-replies contract section must still be appended: {prompt}"
    );
    assert!(
        !prompt.starts_with(CONST_PROMPT_OPENING),
        "the const prompt must NOT leak through when a persona is set"
    );
}

// ---------------------------------------------------------------------------
// Host tool-hook injection (`.tool_hooks(..)` → every turn's registry)
// ---------------------------------------------------------------------------

/// A spy `ToolHook` that records whether its `pre_call`/`post_call` fired. This
/// is the seam Big Smooth's narc-judge + auto-mode ride on: an injected hook must
/// observe every tool call the turn makes.
#[derive(Default)]
struct SpyHook {
    pre_fired: std::sync::atomic::AtomicBool,
    post_fired: std::sync::atomic::AtomicBool,
    seen_tool: std::sync::Mutex<Option<String>>,
}

#[async_trait]
impl smooth_operator_core::tool::ToolHook for SpyHook {
    async fn pre_call(&self, call: &smooth_operator_core::tool::ToolCall) -> anyhow::Result<()> {
        self.pre_fired
            .store(true, std::sync::atomic::Ordering::SeqCst);
        *self.seen_tool.lock().unwrap() = Some(call.name.clone());
        Ok(())
    }
    async fn post_call(
        &self,
        _call: &smooth_operator_core::tool::ToolCall,
        _result: &mut smooth_operator_core::tool::ToolResult,
    ) -> anyhow::Result<()> {
        self.post_fired
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// An injected `ToolHook` fires `pre_call` + `post_call` for every tool the turn
/// executes. The mock is scripted to call the host tool (turn 1) then answer
/// (turn 2); the spy hook, installed via `TurnRequest::tool_hooks`, must observe
/// the call.
#[tokio::test]
async fn injected_tool_hook_observes_tool_calls() {
    use smooth_operator_core::tool::ToolHook;

    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());

    // Turn 1: the model calls `host_crm_lookup`. Turn 2: it answers.
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "host_crm_lookup".into(),
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
        StreamEvent::Delta {
            content: "Done.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);

    let provider = Arc::new(StubProvider {
        seen: Arc::new(std::sync::Mutex::new(SeenCtx::default())),
    });
    let spy = Arc::new(SpyHook::default());

    let (tx, mut rx): (_, UnboundedReceiver<Value>) = unbounded_channel();
    runner::run_streaming_turn(
        TurnRequest {
            demo_tools: false,
            storage,
            llm: mock_llm(),
            max_iterations: 4,
            conversation_id: "conv-hook",
            request_id: "req-hook",
            user_message: "look me up",
            images: vec![],
            files: vec![],
            model_max_output: None,
            access: AccessContext::anonymous(),
            llm_provider: Some(Arc::new(mock)),
            executor: None,
            reranker: None,
            confirmation: None,
            interactions: None,
            tool_provider: Some(provider),
            tool_hooks: vec![spy.clone() as Arc<dyn ToolHook>],
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
            request_metadata: None,
        },
        &tx,
    )
    .await
    .expect("run_streaming_turn");

    drop(tx);
    while rx.recv().await.is_some() {}

    assert!(
        spy.pre_fired.load(std::sync::atomic::Ordering::SeqCst),
        "the injected hook's pre_call must fire on a tool call"
    );
    assert!(
        spy.post_fired.load(std::sync::atomic::Ordering::SeqCst),
        "the injected hook's post_call must fire on a tool call"
    );
    assert_eq!(
        spy.seen_tool.lock().unwrap().as_deref(),
        Some("host_crm_lookup"),
        "the hook must observe the tool that was called"
    );
}
