//! Integration coverage for the per-agent tool authLevel gate + per-tool config
//! delivery (SMOODEV-590), driving the real streaming turn with a scripted LLM.
//!
//! A host `ToolProvider` contributes a `pay` tool that records (a) the per-tool
//! `config` it was handed via [`ToolProviderContext`] and (b) whether it actually
//! executed. The scripted mock makes the model call `pay`, so the auth gate runs
//! at execution time. We assert the reference branches:
//!   - admin tool on a public agent → blocked, never executes;
//!   - end_user on public, unauthenticated → blocked; authenticated → executes;
//!   - internal agent → auto-satisfied, executes;
//!   - the enabledTools `config` reaches the tool.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc::unbounded_channel;

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator::agent_config::{AuthGateHook, AuthLevel, Visibility};
use smooth_operator::tool_provider::{ToolProvider, ToolProviderContext};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::StreamEvent;
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::{LlmConfig, Tool, ToolSchema};

use smooth_operator_server::runner::{self, TurnRequest};

const CONVERSATION_ID: &str = "conv-auth-1";
const REQUEST_ID: &str = "req-auth-1";
const TOOL: &str = "pay";

fn mock_llm() -> LlmConfig {
    LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o")
}

/// A tool that records whether it executed + the config it was constructed with.
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
        Ok("charged".into())
    }
}

/// Provider that returns the recording `pay` tool and captures the per-tool
/// config it saw on the context (registry.ts `toolSpecificConfig` parity).
struct RecordingProvider {
    executed: Arc<AtomicBool>,
    seen_config: Arc<Mutex<Option<Value>>>,
}

#[async_trait]
impl ToolProvider for RecordingProvider {
    async fn tools_for(&self, ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
        *self.seen_config.lock().unwrap() = ctx.tool_specific_config.get(TOOL).cloned();
        vec![Arc::new(RecordingTool {
            executed: self.executed.clone(),
        })]
    }
}

/// Mock that turn-1 streams a `pay` tool call, turn-2 streams the final answer.
fn scripted_mock() -> MockLlmClient {
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
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
        StreamEvent::Delta {
            content: "Done.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);
    mock
}

/// Drive one turn with the gate + tool config, returning (executed?, seen_config,
/// all sink events as strings).
async fn run(
    auth_gate: Option<AuthGateHook>,
    tool_configs: Option<std::collections::HashMap<String, Value>>,
) -> (bool, Option<Value>, Vec<String>) {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());
    let executed = Arc::new(AtomicBool::new(false));
    let seen_config = Arc::new(Mutex::new(None));
    let provider = Arc::new(RecordingProvider {
        executed: executed.clone(),
        seen_config: seen_config.clone(),
    });
    let (tx, mut rx) = unbounded_channel::<Value>();

    runner::run_streaming_turn(
        TurnRequest {
            demo_tools: false,
            storage,
            llm: mock_llm(),
            max_iterations: 4,
            conversation_id: CONVERSATION_ID,
            request_id: REQUEST_ID,
            user_message: "pay my bill",
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
            auth_gate,
            tool_configs,
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
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev.to_string());
    }
    let seen = seen_config.lock().unwrap().clone();
    (executed.load(Ordering::SeqCst), seen, events)
}

/// Build a gate that treats `pay` as auth-supporting with the given level +
/// visibility + session-auth.
fn gate(level: AuthLevel, visibility: Visibility, authed: bool) -> AuthGateHook {
    let levels = [(TOOL.to_string(), level)].into_iter().collect();
    let supporting = [TOOL.to_string()].into_iter().collect();
    AuthGateHook::new(levels, visibility, authed, supporting)
}

#[tokio::test]
async fn admin_tool_on_public_agent_is_blocked() {
    let (executed, _cfg, events) = run(
        Some(gate(AuthLevel::Admin, Visibility::Public, false)),
        None,
    )
    .await;
    assert!(!executed, "admin tool must NOT execute on a public agent");
    assert!(
        events
            .iter()
            .any(|e| e.contains("requires admin authentication")),
        "the reference admin refusal should reach the model: {events:?}"
    );
}

#[tokio::test]
async fn end_user_on_public_unauthenticated_is_blocked() {
    let (executed, _cfg, events) = run(
        Some(gate(AuthLevel::EndUser, Visibility::Public, false)),
        None,
    )
    .await;
    assert!(
        !executed,
        "end_user tool must NOT execute when unauthenticated"
    );
    assert!(
        events.iter().any(|e| e.contains("verify your identity")),
        "identity-verification refusal should reach the model: {events:?}"
    );
}

#[tokio::test]
async fn end_user_unauthenticated_refusal_is_recorded_for_otp() {
    // The gate the server keeps a handle to should record the refused end_user
    // tool during a real turn, so the post-turn OTP offer can trigger. The clone
    // shares the Arc-backed flag with the instance installed in the turn.
    let g = gate(AuthLevel::EndUser, Visibility::Public, false);
    let observed = g.clone();
    let (executed, _cfg, _events) = run(Some(g), None).await;
    assert!(!executed, "end_user tool must not execute unauthenticated");
    assert_eq!(
        observed.otp_refused_tool(),
        Some(TOOL.to_string()),
        "the refused end_user tool should be recorded for the OTP offer"
    );
}

#[tokio::test]
async fn admin_refusal_is_not_recorded_for_otp() {
    // An admin refusal is not OTP-remediable → the flag stays clear so the
    // server never offers OTP for it.
    let g = gate(AuthLevel::Admin, Visibility::Public, false);
    let observed = g.clone();
    let (_executed, _cfg, _events) = run(Some(g), None).await;
    assert_eq!(observed.otp_refused_tool(), None);
}

#[tokio::test]
async fn end_user_on_public_authenticated_executes() {
    let (executed, _cfg, _events) = run(
        Some(gate(AuthLevel::EndUser, Visibility::Public, true)),
        None,
    )
    .await;
    assert!(executed, "authenticated end_user tool should execute");
}

#[tokio::test]
async fn internal_agent_auto_satisfies_admin() {
    let (executed, _cfg, _events) = run(
        Some(gate(AuthLevel::Admin, Visibility::Internal, false)),
        None,
    )
    .await;
    assert!(executed, "internal agent auto-satisfies admin auth");
}

#[tokio::test]
async fn no_gate_executes_and_per_tool_config_reaches_the_tool() {
    let mut configs = std::collections::HashMap::new();
    configs.insert(TOOL.to_string(), json!({ "account": "acct_42" }));
    let (executed, seen, _events) = run(None, Some(configs)).await;
    assert!(executed, "ungated tool executes");
    assert_eq!(
        seen,
        Some(json!({ "account": "acct_42" })),
        "the enabledTools config should be delivered to the tool"
    );
}
