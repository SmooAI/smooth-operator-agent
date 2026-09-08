//! Telemetry coverage for the PRODUCTION streaming path (`run_streaming_turn`).
//!
//! The reference server drives turns through `runner::run_streaming_turn` (not
//! `KnowledgeChatRuntime::run_turn`), so this asserts — via a capturing `tracing`
//! subscriber, no live OTLP collector — that a real streaming turn emits:
//!
//! 1. A `gen_ai.chat` turn span carrying `gen_ai.system`, `gen_ai.request.model`,
//!    `gen_ai.conversation.id`, `gen_ai.agent.name`, and `smooai.org_id` (the
//!    monorepo TS chat handler's org attribute, so the studio groups by org).
//! 2. A child `gen_ai.tool` span carrying `gen_ai.tool.name` and the (redacted)
//!    `gen_ai.tool.call.arguments` the model passed — plus its OWN copy of
//!    `gen_ai.system`, `gen_ai.operation.name`, `gen_ai.conversation.id` and
//!    `smooai.org_id`, since the OTLP ingest does not inherit attributes from a
//!    parent span.
//! 3. `gen_ai.usage.cost_usd` on the turn span when the gateway reported a
//!    cost — and NO such attribute when it didn't, so an unpriced turn reads as
//!    "not measured" instead of a confident `$0.00`.

#![allow(clippy::missing_panics_doc)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::sync::mpsc::unbounded_channel;

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::llm::{StreamEvent, Usage};
use smooth_operator_core::llm_provider::MockLlmClient;
use smooth_operator_core::{Document, DocumentType, LlmConfig};
use smooth_operator_server::runner::{self, TurnRequest};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::Subscriber;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// One captured span: its name + flattened field values (creation + `record`).
#[derive(Debug, Clone, Default)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
}

type SpanSink = Arc<Mutex<Vec<CapturedSpan>>>;

/// Records every span's name and string/int fields into a shared `Vec` so a test
/// can assert on GenAI attributes without a live OTLP collector.
struct CapturingLayer {
    sink: SpanSink,
    index: Arc<Mutex<HashMap<u64, usize>>>,
}

struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

impl Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

impl<S> Layer<S> for CapturingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
        let mut fields = HashMap::new();
        attrs.record(&mut FieldVisitor(&mut fields));
        let captured = CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields,
        };
        let mut sink = self.sink.lock().expect("sink poisoned");
        let idx = sink.len();
        sink.push(captured);
        self.index
            .lock()
            .expect("index poisoned")
            .insert(id.into_u64(), idx);
    }

    fn on_record(&self, id: &Id, values: &tracing::span::Record<'_>, _ctx: Context<'_, S>) {
        let idx = {
            let index = self.index.lock().expect("index poisoned");
            index.get(&id.into_u64()).copied()
        };
        if let Some(idx) = idx {
            let mut sink = self.sink.lock().expect("sink poisoned");
            if let Some(entry) = sink.get_mut(idx) {
                values.record(&mut FieldVisitor(&mut entry.fields));
            }
        }
    }
}

fn seeded_storage() -> Arc<dyn StorageAdapter> {
    let storage = Arc::new(InMemoryStorageAdapter::new());
    storage
        .knowledge()
        .ingest(Document::new(
            "Returns are accepted within 30 days for a full refund.",
            "policies/returns.md",
            DocumentType::Documentation,
        ))
        .expect("ingest doc");
    storage
}

fn mock_llm() -> LlmConfig {
    LlmConfig::openrouter("not-a-real-key").with_model("openai/gpt-4o")
}

/// A `TurnRequest` with every knob at its inert default, for tests that only
/// care about a couple of fields and fill the rest with `..base_turn_request()`.
fn base_turn_request() -> TurnRequest<'static> {
    TurnRequest {
        demo_tools: false,
        storage: seeded_storage(),
        llm: mock_llm(),
        max_iterations: 4,
        conversation_id: "conv-otel-srv",
        request_id: "req-otel-srv",
        user_message: "what is the return policy?",
        model_max_output: None,
        access: AccessContext::anonymous(),
        llm_provider: None,
        executor: None,
        reranker: None,
        confirmation: None,
        interactions: None,
        tool_provider: None,
        tool_hooks: vec![],
        system_prompt: None,
        org_id: Some("org-telemetry".to_string()),
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
    }
}

#[tokio::test]
async fn streaming_turn_emits_gen_ai_spans_with_org_and_tool_args() {
    let sink: SpanSink = Arc::new(Mutex::new(Vec::new()));
    let layer = CapturingLayer {
        sink: Arc::clone(&sink),
        index: Arc::new(Mutex::new(HashMap::new())),
    };
    // `#[tokio::test]` uses the current-thread runtime, so the spawned event
    // translator polls on this same thread and sees the thread-local subscriber.
    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    // Script the mock for the STREAMING path: turn 1 streams a knowledge_search
    // tool call (with args), turn 2 streams the final answer. (The non-streaming
    // `push_tool_call` helper doesn't drive `run_with_channel`.)
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_kb_1".into(),
            name: "knowledge_search".into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 0,
            arguments_chunk: json!({ "query": "return policy refund window" }).to_string(),
        },
        StreamEvent::Cost { usd: 0.0042 },
        StreamEvent::ResponseId {
            id: "chatcmpl-turn-1".into(),
        },
        StreamEvent::Done {
            finish_reason: "tool_calls".into(),
        },
    ])
    .push_stream(vec![
        StreamEvent::Delta {
            content: "Items are accepted within 30 days for a full refund.".into(),
        },
        StreamEvent::Cost { usd: 0.0011 },
        // The tracker keeps the LAST call's id, so this is the one recorded.
        StreamEvent::ResponseId {
            id: "chatcmpl-turn-2".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);

    let (tx, mut rx) = unbounded_channel::<serde_json::Value>();
    runner::run_streaming_turn(
        TurnRequest {
            llm_provider: Some(Arc::new(mock.clone())),
            ..base_turn_request()
        },
        &tx,
    )
    .await
    .expect("run_streaming_turn");
    drop(tx);
    while rx.try_recv().is_ok() {}

    let spans = sink.lock().expect("sink poisoned").clone();

    // (1) The turn span carries system, model, conversation, agent, and org.
    let chat = spans
        .iter()
        .find(|s| s.name == "gen_ai.chat")
        .unwrap_or_else(|| panic!("expected a `gen_ai.chat` span; got: {spans:#?}"));
    assert_eq!(
        chat.fields.get("gen_ai.system").map(String::as_str),
        Some("smooth-operator")
    );
    assert_eq!(
        chat.fields.get("gen_ai.request.model").map(String::as_str),
        Some("openai/gpt-4o")
    );
    assert_eq!(
        chat.fields
            .get("gen_ai.conversation.id")
            .map(String::as_str),
        Some("conv-otel-srv")
    );
    assert_eq!(
        chat.fields.get("gen_ai.agent.name").map(String::as_str),
        Some("smooth-agent-chat")
    );
    assert_eq!(
        chat.fields.get("smooai.org_id").map(String::as_str),
        Some("org-telemetry"),
        "smooai.org_id groups the studio by org; span fields: {:?}",
        chat.fields
    );

    // (2) A child tool span with the tool name + redacted arguments.
    let tool = spans
        .iter()
        .find(|s| s.name == "gen_ai.tool")
        .unwrap_or_else(|| panic!("expected a `gen_ai.tool` span; got: {spans:#?}"));
    assert_eq!(
        tool.fields.get("gen_ai.tool.name").map(String::as_str),
        Some("knowledge_search")
    );
    let args = tool
        .fields
        .get("gen_ai.tool.call.arguments")
        .map(String::as_str)
        .unwrap_or_default();
    assert!(
        args.contains("return policy refund window"),
        "tool arguments should carry the model's query; got: {args:?}"
    );

    // (3) The tool span repeats the identifiers itself. The OTLP ingest merges
    // resource attrs + THIS span's attrs with NO parent inheritance, so a tool
    // span without these cannot be joined to its conversation — and without
    // `gen_ai.system` it fails the ingest's LLM-event gate outright.
    assert_eq!(
        tool.fields.get("gen_ai.system").map(String::as_str),
        Some("smooth-operator"),
        "tool span needs its own gen_ai.system or the ingest drops it; fields: {:?}",
        tool.fields
    );
    assert_eq!(
        tool.fields.get("gen_ai.operation.name").map(String::as_str),
        Some("tool"),
        "must be exactly `tool` — the ingest takes this attribute verbatim and \
         its queries filter on operation_name = 'tool'; fields: {:?}",
        tool.fields
    );
    assert_eq!(
        tool.fields
            .get("gen_ai.conversation.id")
            .map(String::as_str),
        Some("conv-otel-srv"),
        "tool span must be joinable to its conversation; fields: {:?}",
        tool.fields
    );
    assert_eq!(
        tool.fields.get("smooai.org_id").map(String::as_str),
        Some("org-telemetry"),
        "tool span must carry the org; fields: {:?}",
        tool.fields
    );

    // (4) Per-turn cost from the gateway's `x-litellm-response-cost` header,
    // accumulated across both LLM calls in the turn (0.0042 + 0.0011).
    let cost: f64 = chat
        .fields
        .get("gen_ai.usage.cost_usd")
        .unwrap_or_else(|| panic!("expected gen_ai.usage.cost_usd; fields: {:?}", chat.fields))
        .parse()
        .expect("cost is a number");
    assert!(
        (cost - 0.0053).abs() < 1e-9,
        "expected the two streams' gateway costs summed; got {cost}"
    );
    assert_eq!(
        chat.fields
            .get("gen_ai.usage.cost_source")
            .map(String::as_str),
        Some("gateway"),
        "a gateway-reported cost must be labelled authoritative; fields: {:?}",
        chat.fields
    );

    // (5) The join key to LiteLLM's spend log, which carries the gateway's own
    // dollars AND real token counts — the cross-check on everything above.
    assert_eq!(
        chat.fields.get("gen_ai.response.id").map(String::as_str),
        Some("chatcmpl-turn-2"),
        "expected the FINAL call's response id; fields: {:?}",
        chat.fields
    );
}

/// A model the gateway can't price yields `cost_usd = 0` — from LiteLLM
/// answering `x-litellm-response-cost: 0` (which core maps to `None`) and the
/// local `ModelPricing` fallback pricing an unrecognised model at 0. That zero
/// must NOT be exported: a consumer has to be able to say "not measured"
/// instead of rendering a confident `$0.00` on a turn that genuinely cost money.
///
/// Token usage still has to land, so this also proves the attribute is absent
/// because of the zero check rather than because the span recorded nothing.
#[tokio::test]
async fn unpriced_turn_omits_cost_rather_than_recording_zero() {
    let sink: SpanSink = Arc::new(Mutex::new(Vec::new()));
    let layer = CapturingLayer {
        sink: Arc::clone(&sink),
        index: Arc::new(Mutex::new(HashMap::new())),
    };
    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    // Real token usage, no `Cost` event — exactly what an unpriced model on the
    // gateway produces.
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::Delta {
            content: "Items are accepted within 30 days for a full refund.".into(),
        },
        StreamEvent::Usage(Usage {
            prompt_tokens: 1234,
            completion_tokens: 56,
            total_tokens: 1290,
            cached_tokens: 0,
        }),
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);

    let (tx, mut rx) = unbounded_channel::<serde_json::Value>();
    runner::run_streaming_turn(
        TurnRequest {
            // `unpriced-local-model` is in no `ModelPricing` table entry, so the
            // local fallback prices it at $0.
            llm: LlmConfig::openrouter("not-a-real-key").with_model("unpriced-local-model"),
            llm_provider: Some(Arc::new(mock.clone())),
            conversation_id: "conv-otel-unpriced",
            request_id: "req-otel-unpriced",
            ..base_turn_request()
        },
        &tx,
    )
    .await
    .expect("run_streaming_turn");
    drop(tx);
    while rx.try_recv().is_ok() {}

    let spans = sink.lock().expect("sink poisoned").clone();
    let chat = spans
        .iter()
        .find(|s| s.name == "gen_ai.chat")
        .unwrap_or_else(|| panic!("expected a `gen_ai.chat` span; got: {spans:#?}"));

    assert_eq!(
        chat.fields
            .get("gen_ai.usage.input_tokens")
            .map(String::as_str),
        Some("1234"),
        "usage must still land, or the cost assertion below proves nothing; fields: {:?}",
        chat.fields
    );
    assert!(
        !chat.fields.contains_key("gen_ai.usage.cost_usd"),
        "an unpriced turn must leave gen_ai.usage.cost_usd ABSENT, not 0 — a \
         missing price must never read as free; fields: {:?}",
        chat.fields
    );
    // …and says WHY, with the same attribute name + value the TS lane emits, so
    // a consumer never special-cases per engine. "unpriced" is actionable:
    // someone has to price the model.
    assert_eq!(
        chat.fields
            .get("smooai.gen_ai.cost_unavailable")
            .map(String::as_str),
        Some("unpriced"),
        "absence alone makes a consumer infer the reason; fields: {:?}",
        chat.fields
    );
}

/// The prod signature, four for four across every chat-ws turn ever recorded:
/// `input_tokens = 0` with a plausible-looking output count.
///
/// Its cause is not this crate — core's `collect_stream` fabricates the whole
/// usage struct when the gateway sends no usage chunk (LiteLLM drops it for
/// `smooth-*` aliases), hardcoding `prompt_tokens = 0` and estimating
/// `completion_tokens` as `content.len() / 4`. So the "plausible" output count
/// was never a measurement either. Until core stops fabricating, the honest
/// move is to export neither. Pearl th-126fe6.
#[tokio::test]
async fn fabricated_usage_omits_both_token_counts() {
    let sink: SpanSink = Arc::new(Mutex::new(Vec::new()));
    let layer = CapturingLayer {
        sink: Arc::clone(&sink),
        index: Arc::new(Mutex::new(HashMap::new())),
    };
    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    // No `StreamEvent::Usage` at all — exactly what the gateway sends today.
    // Core will fabricate: prompt 0, completion ≈ len/4.
    let mock = MockLlmClient::new();
    mock.push_stream(vec![
        StreamEvent::Delta {
            content: "Items are accepted within 30 days for a full refund.".into(),
        },
        StreamEvent::Done {
            finish_reason: "stop".into(),
        },
    ]);

    let (tx, mut rx) = unbounded_channel::<serde_json::Value>();
    runner::run_streaming_turn(
        TurnRequest {
            llm_provider: Some(Arc::new(mock.clone())),
            conversation_id: "conv-otel-fabricated",
            request_id: "req-otel-fabricated",
            ..base_turn_request()
        },
        &tx,
    )
    .await
    .expect("run_streaming_turn");
    drop(tx);
    while rx.try_recv().is_ok() {}

    let spans = sink.lock().expect("sink poisoned").clone();
    let chat = spans
        .iter()
        .find(|s| s.name == "gen_ai.chat")
        .unwrap_or_else(|| panic!("expected a `gen_ai.chat` span; got: {spans:#?}"));

    assert!(
        !chat.fields.contains_key("gen_ai.usage.input_tokens"),
        "input_tokens = 0 is impossible on a grounded turn — it must be ABSENT, \
         not 0; fields: {:?}",
        chat.fields
    );
    assert!(
        !chat.fields.contains_key("gen_ai.usage.output_tokens"),
        "the output count beside a fabricated input is core's content.len()/4 \
         estimate, not a measurement — shipping it next to a dollar figure \
         would look authoritative; fields: {:?}",
        chat.fields
    );

    // Cost is still judged independently of the counts (separate channels), and
    // `openai/gpt-4o` IS in the local `ModelPricing` table — so this turn does
    // get a locally-derived cost computed against a zero input count. What is
    // no longer ambiguous is WHERE it came from: provenance now says so out
    // loud, which is what closes the gap this test used to merely document.
    assert!(
        chat.fields.contains_key("gen_ai.usage.cost_usd"),
        "fields: {:?}",
        chat.fields
    );
    assert_eq!(
        chat.fields
            .get("gen_ai.usage.cost_source")
            .map(String::as_str),
        Some("estimated"),
        "a locally-priced turn must be labelled an estimate, not passed off as \
         the gateway's figure; fields: {:?}",
        chat.fields
    );
}
